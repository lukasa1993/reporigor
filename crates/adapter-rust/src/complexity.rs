use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::Span;
use quote::ToTokens;
use syn::{
    spanned::Spanned,
    visit::{self, Visit},
    Attribute, BinOp, Block, Expr, ExprBinary, FnArg, Meta, Pat, PatIdent, Signature, Stmt,
};

use reporigor_core::{
    CoverageSpan, FeatureRecord, FunctionRecord, IdentifierCountRecord, Language, ModuleRecord,
    RepositorySemantics, SymbolVisibility, TestRecord, TraitImplementationRecord, UnreachableRecord,
};

use crate::scope::{
    attributes_target_gated, combine_cfg_evidence, items_reflection_reachable, module_framework_managed,
    module_reflection_reachable, module_visibility, parse_meta_list_for_analysis, CfgContext, ScopedFile,
};
use crate::syntax::{
    attribute_ranges, attributes_contain, expr_attrs, inactive_file_ranges, item_attrs, AttributeRangeRoot,
};

#[derive(Debug, Default)]
pub(crate) struct StructuralAnalysis {
    pub functions: Vec<FunctionRecord>,
    pub repository: RepositorySemantics,
}

#[derive(Clone, Copy)]
enum CfgSelection {
    Production,
    Tests,
}

#[derive(Clone, Copy)]
struct RealizableCfg<'a> {
    cfg: &'a CfgContext,
    selection: CfgSelection,
}

impl RealizableCfg<'_> {
    fn attrs_active(self, attrs: &[Attribute]) -> bool {
        match self.selection {
            CfgSelection::Production => self.cfg.attrs_active_in_production(attrs),
            CfgSelection::Tests => self.cfg.attrs_active_in_tests(attrs),
        }
    }
}

struct MetricsVisitor<'a> {
    complexity: u32,
    depth: u32,
    max_depth: u32,
    statements: u32,
    cfg: RealizableCfg<'a>,
}

impl<'a> MetricsVisitor<'a> {
    fn for_block(block: &Block, cfg: RealizableCfg<'a>) -> Self {
        let mut visitor = Self {
            complexity: 1,
            depth: 0,
            max_depth: 0,
            statements: 0,
            cfg,
        };
        visitor.visit_block(block);
        visitor
    }

    fn enter_control(&mut self) {
        self.enter_paths(1);
    }

    fn enter_paths(&mut self, paths: u32) {
        self.complexity = self.complexity.saturating_add(paths);
        self.depth = self.depth.saturating_add(1);
        self.max_depth = self.max_depth.max(self.depth);
    }

    fn exit_control(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}

macro_rules! skip_nested_executable_methods {
    () => {
        fn visit_item(&mut self, _node: &'ast syn::Item) {}

        fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {}
    };
}

macro_rules! cfg_filtered_expr_method {
    () => {
        fn visit_expr(&mut self, node: &'ast Expr) {
            if self.cfg.attrs_active(expr_attrs(node)) {
                visit::visit_expr(self, node);
            }
        }
    };
}

impl<'ast> Visit<'ast> for MetricsVisitor<'_> {
    skip_nested_executable_methods!();

    fn visit_stmt(&mut self, node: &'ast Stmt) {
        if !self.cfg.attrs_active(stmt_attrs(node)) {
            return;
        }
        self.statements = self.statements.saturating_add(1);
        visit::visit_stmt(self, node);
    }

    cfg_filtered_expr_method!();

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.enter_control();
        visit::visit_expr_if(self, node);
        self.exit_control();
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.enter_control();
        visit::visit_expr_for_loop(self, node);
        self.exit_control();
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.enter_control();
        visit::visit_expr_while(self, node);
        self.exit_control();
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.enter_control();
        visit::visit_expr_loop(self, node);
        self.exit_control();
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        // McCabe complexity counts independent alternatives, not syntax
        // arms. The first active arm is the existing path; each additional
        // arm adds one path. A wildcard/default arm is still an alternative.
        let alternatives = node
            .arms
            .iter()
            .filter(|arm| self.cfg.attrs_active(&arm.attrs))
            .count()
            .saturating_sub(1);
        self.enter_paths(u32::try_from(alternatives).unwrap_or(u32::MAX));
        visit::visit_expr_match(self, node);
        self.exit_control();
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if !self.cfg.attrs_active(&node.attrs) {
            return;
        }
        visit::visit_arm(self, node);
    }

    fn visit_pat_guard(&mut self, node: &'ast syn::PatGuard) {
        self.complexity = self.complexity.saturating_add(1);
        visit::visit_pat_guard(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        if matches!(node.op, BinOp::And(_) | BinOp::Or(_)) {
            self.complexity = self.complexity.saturating_add(1);
        }
        visit::visit_expr_binary(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if !self.cfg.attrs_active(&node.attrs) {
            return;
        }
        if node.init.as_ref().is_some_and(|init| init.diverge.is_some()) {
            self.complexity = self.complexity.saturating_add(1);
        }
        visit::visit_local(self, node);
    }

    fn visit_expr_try(&mut self, node: &'ast syn::ExprTry) {
        self.complexity = self.complexity.saturating_add(1);
        visit::visit_expr_try(self, node);
    }
}

fn stmt_attrs(statement: &Stmt) -> &[Attribute] {
    match statement {
        Stmt::Local(local) => &local.attrs,
        Stmt::Item(item) => item_attrs(item),
        Stmt::Expr(expression, _) => expr_attrs(expression),
        Stmt::Macro(statement) => &statement.attrs,
    }
}

struct LocalVisitor<'a> {
    names: BTreeSet<String>,
    cfg: RealizableCfg<'a>,
}

macro_rules! cfg_filtered_visit_methods {
    () => {
        skip_nested_executable_methods!();

        cfg_filtered_node_method!(visit_local, syn::Local, visit::visit_local);
        cfg_filtered_node_method!(visit_arm, syn::Arm, visit::visit_arm);
        cfg_filtered_node_method!(visit_stmt_macro, syn::StmtMacro, visit::visit_stmt_macro);

        cfg_filtered_expr_method!();
    };
}

macro_rules! cfg_filtered_node_method {
    ($method:ident, $node:ty, $visit:path) => {
        fn $method(&mut self, node: &'ast $node) {
            if self.cfg.attrs_active(&node.attrs) {
                $visit(self, node);
            }
        }
    };
}

impl<'ast> Visit<'ast> for LocalVisitor<'_> {
    fn visit_pat_ident(&mut self, node: &'ast PatIdent) {
        self.names.insert(node.ident.to_string());
        visit::visit_pat_ident(self, node);
    }

    cfg_filtered_visit_methods!();
}

fn collect_pattern_names(pattern: &Pat, names: &mut BTreeSet<String>, cfg: RealizableCfg<'_>) {
    let mut visitor = LocalVisitor {
        names: std::mem::take(names),
        cfg,
    };
    visitor.visit_pat(pattern);
    *names = visitor.names;
}

fn argument_attrs(argument: &FnArg) -> &[Attribute] {
    match argument {
        FnArg::Receiver(receiver) => &receiver.attrs,
        FnArg::Typed(argument) => &argument.attrs,
    }
}

fn local_names(signature: &Signature, block: &Block, cfg: RealizableCfg<'_>) -> BTreeSet<String> {
    let mut visitor = LocalVisitor {
        names: BTreeSet::new(),
        cfg,
    };
    for argument in &signature.inputs {
        if !cfg.attrs_active(argument_attrs(argument)) {
            continue;
        }
        match argument {
            FnArg::Receiver(_) => {
                visitor.names.insert("self".to_string());
            }
            FnArg::Typed(argument) => collect_pattern_names(&argument.pat, &mut visitor.names, cfg),
        }
    }
    visitor.visit_block(block);
    visitor.names
}

struct ReferenceVisitor<'a> {
    names: BTreeSet<String>,
    cfg: RealizableCfg<'a>,
}

impl<'ast> Visit<'ast> for ReferenceVisitor<'_> {
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        self.record_path(&node.path);
        if let Some(qself) = &node.qself {
            if let syn::Type::Path(path) = qself.ty.as_ref() {
                self.record_path(&path.path);
            }
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.names.insert(format!("method::{}", node.method));
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_field(&mut self, node: &'ast syn::ExprField) {
        if let syn::Member::Named(field) = &node.member {
            self.names.insert(format!("field::{field}"));
        }
        visit::visit_expr_field(self, node);
    }

    cfg_filtered_visit_methods!();
}

impl ReferenceVisitor<'_> {
    fn record_path(&mut self, path: &syn::Path) {
        let path = path_segments(path).join("::");
        if !path.is_empty() {
            self.names.insert(path);
        }
    }
}

fn nonlocal_references(block: &Block, locals: &BTreeSet<String>, cfg: RealizableCfg<'_>) -> BTreeSet<String> {
    let mut visitor = ReferenceVisitor {
        names: BTreeSet::new(),
        cfg,
    };
    visitor.visit_block(block);
    visitor
        .names
        .retain(|reference| reference.contains("::") || !locals.contains(reference.as_str()));
    visitor.names
}

fn compact_tokens(value: &impl ToTokens) -> String {
    value.to_token_stream().to_string().replace(' ', "")
}

fn stable_trait_symbol(
    package: &str,
    module_prefix: &str,
    path: &syn::Path,
    local_traits: &BTreeSet<String>,
) -> String {
    let segments = path_segments(path);
    local_trait_symbol(package, module_prefix, &segments, local_traits)
        .or_else(|| rooted_trait_symbol(package, module_prefix, &segments))
        .unwrap_or_else(|| compact_tokens(path))
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn local_trait_symbol(
    package: &str,
    module_prefix: &str,
    segments: &[String],
    local_traits: &BTreeSet<String>,
) -> Option<String> {
    let first = segments.first()?;
    (segments.len() == 1 && local_traits.contains(first))
        .then(|| stable_qualify(package, module_prefix, first))
}

#[derive(Clone, Copy)]
enum TraitPathRoot {
    Crate,
    Relative(usize),
}

fn trait_path_root(segments: &[String]) -> Option<(TraitPathRoot, usize)> {
    let first = segments.first()?.as_str();
    match first {
        "crate" => Some((TraitPathRoot::Crate, 1)),
        "self" => Some((TraitPathRoot::Relative(0), 1)),
        "super" => {
            let count = segments.iter().take_while(|segment| *segment == "super").count();
            Some((TraitPathRoot::Relative(count), count))
        }
        _ => None,
    }
}

fn rooted_trait_symbol(package: &str, module_prefix: &str, segments: &[String]) -> Option<String> {
    let (root, remainder) = trait_path_root(segments)?;
    let mut owner = module_prefix
        .split("::")
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    apply_trait_path_root(&mut owner, root);
    let mut qualified = vec![package.to_string()];
    qualified.extend(owner);
    qualified.extend(segments[remainder..].iter().cloned());
    Some(qualified.join("::"))
}

fn apply_trait_path_root(owner: &mut Vec<String>, root: TraitPathRoot) {
    match root {
        TraitPathRoot::Crate => owner.clear(),
        TraitPathRoot::Relative(depth) => owner.truncate(owner.len().saturating_sub(depth)),
    }
}

fn slice_span(source: &str, span: Span) -> String {
    source
        .get(span.byte_range())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn qualify(prefix: &str, local: &str) -> String {
    if prefix.is_empty() {
        local.to_string()
    } else if local.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}::{local}")
    }
}

fn stable_qualify(package: &str, module: &str, local: &str) -> String {
    qualify(&qualify(package, module), local)
}

fn line(value: usize) -> u32 {
    u32::try_from(value.max(1)).unwrap_or(u32::MAX)
}

fn meta_requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => list_requires_test(list, false),
        Meta::List(list) if list.path.is_ident("any") => list_requires_test(list, true),
        Meta::NameValue(_) | Meta::List(_) => false,
    }
}

fn list_requires_test(list: &syn::MetaList, every: bool) -> bool {
    parse_meta_list_for_analysis(list.tokens.clone()).is_some_and(|items| {
        if every {
            !items.is_empty() && items.iter().all(meta_requires_test)
        } else {
            items.iter().any(meta_requires_test)
        }
    })
}

fn attributes_require_test(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("test")
            || attribute.path().is_ident("bench")
            || (attribute.path().is_ident("cfg")
                && match &attribute.meta {
                    Meta::List(list) => syn::parse2::<Meta>(list.tokens.clone())
                        .ok()
                        .is_some_and(|predicate| meta_requires_test(&predicate)),
                    _ => false,
                })
    })
}

fn stable_with_cfg(mut symbol: String, inherited: &str, attributes: &[Attribute]) -> String {
    let evidence = combine_cfg_evidence(inherited, attributes);
    if !evidence.is_empty() {
        symbol.push_str("[cfg:");
        symbol.push_str(&evidence);
        symbol.push(']');
    }
    symbol
}

fn attribute_marker(attribute: &Attribute) -> Option<String> {
    attribute
        .path()
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn entry_point(signature: &Signature, attributes: &[Attribute]) -> bool {
    signature.ident == "main"
        || signature.abi.is_some()
        || attributes
            .iter()
            .filter_map(attribute_marker)
            .any(|name| attribute_implies_entry_point(&name))
}

fn attribute_implies_entry_point(name: &str) -> bool {
    const EXPLICIT: &str = "test|bench|no_mangle|export_name|used|proc_macro|proc_macro_attribute|proc_macro_derive|wasm_bindgen";
    const INERT: &str =
        "allow|cfg|cfg_attr|cold|deny|deprecated|doc|forbid|inline|must_use|track_caller|warn";
    EXPLICIT.split('|').any(|marker| marker == name) || !INERT.split('|').any(|marker| marker == name)
}

struct UnreachableVisitor<'a> {
    source: &'a str,
    file: &'a str,
    symbol: &'a str,
    package: &'a str,
    locals: &'a BTreeSet<String>,
    cfg: &'a CfgContext,
    target_gated: bool,
    ordinals: BTreeMap<String, u32>,
    records: Vec<UnreachableRecord>,
}

impl<'ast> Visit<'ast> for UnreachableVisitor<'_> {
    fn visit_block(&mut self, node: &'ast Block) {
        let mut terminator = None::<(String, bool)>;
        for statement in &node.stmts {
            let attributes = stmt_attrs(statement);
            if !self.cfg.attrs_active(attributes) {
                continue;
            }
            let variant_gated = attributes_contain(attributes, &["cfg", "cfg_attr"]);
            if let Some((after, terminator_gated)) = terminator.as_ref() {
                self.record(statement, after, variant_gated, *terminator_gated);
            }
            if terminator.is_none() {
                terminator =
                    direct_terminator(statement).map(|terminator| (terminator.to_string(), variant_gated));
            }
            visit::visit_stmt(self, statement);
        }
    }

    fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {}

    fn visit_item_fn(&mut self, _node: &'ast syn::ItemFn) {}
}

impl UnreachableVisitor<'_> {
    fn record(&mut self, statement: &Stmt, after: &str, variant_gated: bool, terminator_gated: bool) {
        let normalized =
            crate::tokens::normalize_fragment(self.source, statement.span().byte_range(), self.locals);
        let shape = normalized.join("\0");
        let ordinal = self.ordinals.entry(shape.clone()).or_default();
        let evidence = format!("after={after}\0statement={shape}\0ordinal={ordinal}");
        *ordinal = ordinal.saturating_add(1);
        self.records.push(UnreachableRecord {
            file: self.file.to_string(),
            stable_symbol: self.symbol.to_string(),
            structural_evidence: evidence,
            package: Some(self.package.to_string()),
            target_gated: self.target_gated || variant_gated || terminator_gated,
        });
    }
}

fn direct_terminator(statement: &Stmt) -> Option<&'static str> {
    match statement {
        Stmt::Expr(Expr::Return(_), _) => Some("return"),
        Stmt::Expr(Expr::Break(_), _) => Some("break"),
        Stmt::Expr(Expr::Continue(_), _) => Some("continue"),
        _ => None,
    }
}

#[derive(Default)]
struct NestedExecutableBodyVisitor {
    ranges: Vec<std::ops::Range<usize>>,
    coverage_ranges: Vec<(u32, u32)>,
    coverage_spans: Vec<CoverageSpan>,
}

impl<'ast> Visit<'ast> for NestedExecutableBodyVisitor {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.record(node.span(), node.block.span());
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.record(node.span(), node.body.span());
    }
}

impl NestedExecutableBodyVisitor {
    fn record(&mut self, range: Span, body: Span) {
        self.ranges.push(range.byte_range());
        let span = coverage_span(body);
        self.coverage_ranges.push((span.start_line, span.end_line));
        self.coverage_spans.push(span);
    }
}

fn selected_inactive_ranges(block: &Block, cfg: RealizableCfg<'_>) -> Vec<std::ops::Range<usize>> {
    attribute_ranges(AttributeRangeRoot::Block(block), |attributes| {
        !cfg.attrs_active(attributes)
    })
}

fn owned_normalized_tokens(
    source: &str,
    block: &Block,
    locals: &BTreeSet<String>,
    cfg: RealizableCfg<'_>,
) -> Vec<String> {
    let mut nested = NestedExecutableBodyVisitor::default();
    nested.visit_block(block);
    nested.ranges.extend(selected_inactive_ranges(block, cfg));
    crate::tokens::normalize_fragment_excluding(source, block.span().byte_range(), locals, &nested.ranges)
}

fn nested_coverage_ranges(block: &Block) -> Vec<(u32, u32)> {
    let mut nested = NestedExecutableBodyVisitor::default();
    nested.visit_block(block);
    nested.coverage_ranges.sort_unstable();
    nested.coverage_ranges.dedup();
    nested.coverage_ranges
}

fn nested_coverage_spans(block: &Block) -> Vec<CoverageSpan> {
    let mut nested = NestedExecutableBodyVisitor::default();
    nested.visit_block(block);
    nested.coverage_spans.sort_unstable();
    nested.coverage_spans.dedup();
    nested.coverage_spans
}

fn coverage_span(span: Span) -> CoverageSpan {
    let start = span.start();
    let end = span.end();
    CoverageSpan {
        start_line: line(start.line),
        start_column: u32::try_from(start.column).unwrap_or(u32::MAX).saturating_add(1),
        end_line: line(end.line.max(start.line)),
        end_column: u32::try_from(end.column).unwrap_or(u32::MAX).saturating_add(1),
    }
}

struct MetricInput<'a> {
    name: String,
    stable_symbol: String,
    file: &'a str,
    signature: &'a Signature,
    span: Span,
    block: &'a Block,
    attributes: &'a [Attribute],
    item_visibility: SymbolVisibility,
    scoped: &'a ScopedFile,
    inherited_cfg: &'a str,
    inherited_target_gated: bool,
    inherited_production_active: bool,
    source: &'a str,
}

struct MetricContext<'a> {
    production: bool,
    realizable_cfg: RealizableCfg<'a>,
    stable_symbol: String,
    target_gated: bool,
    inventory_excluded: bool,
}

fn metric(input: MetricInput<'_>, repository: &mut RepositorySemantics) -> FunctionRecord {
    let context = metric_context(&input);
    let metrics = MetricsVisitor::for_block(input.block, context.realizable_cfg);
    let locals = local_names(input.signature, input.block, context.realizable_cfg);
    let references = nonlocal_references(input.block, &locals, context.realizable_cfg);
    append_unreachable_records(&input, &context, &locals, repository);
    append_test_record(&input, &context, &references, repository);
    build_function_record(input, context, &metrics, &locals, references)
}

fn metric_context<'a>(input: &MetricInput<'a>) -> MetricContext<'a> {
    let production = function_in_production(input);
    let selection = if !production && input.scoped.cfg.attrs_active_in_tests(input.attributes) {
        CfgSelection::Tests
    } else {
        CfgSelection::Production
    };
    let realizable_cfg = RealizableCfg {
        cfg: &input.scoped.cfg,
        selection,
    };
    let stable_symbol = stable_with_cfg(input.stable_symbol.clone(), input.inherited_cfg, input.attributes);
    let target_gated = input.scoped.target_gated
        || input.inherited_target_gated
        || attributes_target_gated(input.attributes);
    let inventory_excluded = target_gated || !production;
    MetricContext {
        production,
        realizable_cfg,
        stable_symbol,
        target_gated,
        inventory_excluded,
    }
}

fn function_in_production(input: &MetricInput<'_>) -> bool {
    !Language::Rust.is_test_path(input.file)
        && input.inherited_production_active
        && input.scoped.cfg.attrs_active_in_production(input.attributes)
        && !attributes_require_test(input.attributes)
}

fn append_unreachable_records(
    input: &MetricInput<'_>,
    context: &MetricContext<'_>,
    locals: &BTreeSet<String>,
    repository: &mut RepositorySemantics,
) {
    let mut unreachable = UnreachableVisitor {
        source: input.source,
        file: input.file,
        symbol: &context.stable_symbol,
        package: &input.scoped.package,
        locals,
        cfg: &input.scoped.cfg,
        target_gated: context.inventory_excluded,
        ordinals: BTreeMap::new(),
        records: Vec::new(),
    };
    unreachable.visit_block(input.block);
    repository.unreachable.extend(unreachable.records);
}

fn append_test_record(
    input: &MetricInput<'_>,
    context: &MetricContext<'_>,
    references: &BTreeSet<String>,
    repository: &mut RepositorySemantics,
) {
    let is_test = input
        .attributes
        .iter()
        .filter_map(attribute_marker)
        .any(|marker| matches!(marker.as_str(), "test" | "bench"));
    if !is_test {
        return;
    }
    let mut markers = input
        .attributes
        .iter()
        .filter_map(attribute_marker)
        .collect::<BTreeSet<_>>();
    markers.insert(input.signature.ident.to_string());
    repository.tests.push(TestRecord {
        stable_symbol: context.stable_symbol.clone(),
        file: input.file.to_string(),
        package: Some(input.scoped.package.clone()),
        referenced_symbols: references.clone(),
        markers,
        target_gated: context.target_gated,
    });
}

fn build_function_record(
    input: MetricInput<'_>,
    context: MetricContext<'_>,
    metrics: &MetricsVisitor<'_>,
    locals: &BTreeSet<String>,
    references: BTreeSet<String>,
) -> FunctionRecord {
    let start = input.span.start();
    let end = input.span.end();
    FunctionRecord {
        language: Language::Rust,
        name: input.name,
        file: input.file.to_string(),
        start_line: line(start.line),
        end_line: line(end.line.max(start.line)),
        complexity: metrics.complexity,
        stable_symbol: context.stable_symbol,
        nesting_depth: metrics.max_depth,
        statement_count: metrics.statements,
        parameter_count: u32::try_from(
            input
                .signature
                .inputs
                .iter()
                .filter(|argument| context.realizable_cfg.attrs_active(argument_attrs(argument)))
                .count(),
        )
        .unwrap_or(u32::MAX),
        normalized_tokens: owned_normalized_tokens(input.source, input.block, locals, context.realizable_cfg),
        references,
        coverage_span: coverage_span(input.span),
        coverage_excluded_ranges: nested_coverage_ranges(input.block),
        coverage_excluded_spans: nested_coverage_spans(input.block),
        visibility: input.item_visibility,
        structural_metrics_reliable: true,
        production: context.production,
        // Active platform/feature variants are reachable through Cargo even
        // when no same-variant Rust call is visible here.
        entry_point: context.inventory_excluded || entry_point(input.signature, input.attributes),
        package: Some(input.scoped.package.clone()),
        coverage: None,
        crap: None,
    }
}

#[derive(Clone, Copy)]
struct CollectionContext<'a> {
    source: &'a str,
    file: &'a str,
    module_prefix: &'a str,
    scoped: &'a ScopedFile,
    inherited_cfg: &'a str,
    inherited_target_gated: bool,
    inherited_production_active: bool,
}

fn collect_items(items: &[syn::Item], context: CollectionContext<'_>, output: &mut StructuralAnalysis) {
    let local_traits = local_trait_names(items);
    for item in items {
        if !context.scoped.cfg.attrs_active(item_attrs(item)) {
            continue;
        }
        collect_item(item, context, &local_traits, output);
    }
}

fn local_trait_names(items: &[syn::Item]) -> BTreeSet<String> {
    items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Trait(item) => Some(item.ident.to_string()),
            _ => None,
        })
        .collect()
}

fn collect_item(
    item: &syn::Item,
    context: CollectionContext<'_>,
    local_traits: &BTreeSet<String>,
    output: &mut StructuralAnalysis,
) {
    match item {
        syn::Item::Fn(function) => collect_function(function, context, output),
        syn::Item::Impl(implementation) => {
            collect_implementation(implementation, context, local_traits, output);
        }
        syn::Item::Trait(trait_item) => collect_trait_defaults(trait_item, context, output),
        syn::Item::Mod(module) => collect_inline_module(module, context, output),
        _ => {}
    }
}

fn collect_function(function: &syn::ItemFn, context: CollectionContext<'_>, output: &mut StructuralAnalysis) {
    let local = function.sig.ident.to_string();
    let input = MetricInput {
        name: qualify(context.module_prefix, &local),
        stable_symbol: stable_qualify(&context.scoped.package, context.module_prefix, &local),
        file: context.file,
        signature: &function.sig,
        span: function.span(),
        block: &function.block,
        attributes: &function.attrs,
        item_visibility: module_visibility(&function.vis),
        scoped: context.scoped,
        inherited_cfg: context.inherited_cfg,
        inherited_target_gated: context.inherited_target_gated,
        inherited_production_active: context.inherited_production_active,
        source: context.source,
    };
    output.functions.push(metric(input, &mut output.repository));
}

struct NestedItemContext {
    cfg: String,
    target_gated: bool,
    production_active: bool,
}

struct MetricIdentity {
    name: String,
    stable_symbol: String,
}

#[derive(Clone, Copy)]
struct FunctionBody<'a> {
    signature: &'a Signature,
    span: Span,
    block: &'a Block,
    attributes: &'a [Attribute],
    visibility: SymbolVisibility,
}

fn nested_item_context(attributes: &[Attribute], context: CollectionContext<'_>) -> NestedItemContext {
    NestedItemContext {
        cfg: combine_cfg_evidence(context.inherited_cfg, attributes),
        target_gated: context.inherited_target_gated || attributes_target_gated(attributes),
        production_active: context.inherited_production_active
            && context.scoped.cfg.attrs_active_in_production(attributes)
            && !attributes_require_test(attributes),
    }
}

fn collect_metric(
    identity: MetricIdentity,
    body: FunctionBody<'_>,
    context: CollectionContext<'_>,
    nested: &NestedItemContext,
    output: &mut StructuralAnalysis,
) {
    let input = MetricInput {
        name: identity.name,
        stable_symbol: identity.stable_symbol,
        file: context.file,
        signature: body.signature,
        span: body.span,
        block: body.block,
        attributes: body.attributes,
        item_visibility: body.visibility,
        scoped: context.scoped,
        inherited_cfg: &nested.cfg,
        inherited_target_gated: nested.target_gated,
        inherited_production_active: nested.production_active,
        source: context.source,
    };
    output.functions.push(metric(input, &mut output.repository));
}

fn collect_implementation(
    implementation: &syn::ItemImpl,
    context: CollectionContext<'_>,
    local_traits: &BTreeSet<String>,
    output: &mut StructuralAnalysis,
) {
    let nested = nested_item_context(&implementation.attrs, context);
    let owners = implementation_owners(
        implementation,
        context,
        &nested,
        local_traits,
        &mut output.repository,
    );
    for member in &implementation.items {
        let syn::ImplItem::Fn(function) = member else {
            continue;
        };
        if !context.scoped.cfg.attrs_active(&function.attrs) {
            continue;
        }
        collect_implementation_function(function, context, &nested, &owners, output);
    }
}

struct ImplementationOwners {
    legacy: String,
    stable: String,
    implements_trait: bool,
}

fn implementation_owners(
    implementation: &syn::ItemImpl,
    context: CollectionContext<'_>,
    nested: &NestedItemContext,
    local_traits: &BTreeSet<String>,
    repository: &mut RepositorySemantics,
) -> ImplementationOwners {
    let display_type = slice_span(context.source, implementation.self_ty.span());
    let stable_type = compact_tokens(&implementation.self_ty);
    let Some((trait_path, _)) = &implementation.trait_ else {
        return ImplementationOwners {
            legacy: display_type,
            stable: stable_type,
            implements_trait: false,
        };
    };
    let trait_name = stable_trait_symbol(
        &context.scoped.package,
        context.module_prefix,
        trait_path,
        local_traits,
    );
    let implementation_symbol = stable_with_cfg(
        stable_qualify(&context.scoped.package, context.module_prefix, &stable_type),
        &nested.cfg,
        &[],
    );
    repository.trait_implementations.push(TraitImplementationRecord {
        trait_symbol: trait_name.clone(),
        implementation_symbol,
        file: context.file.to_string(),
        package: Some(context.scoped.package.clone()),
        target_gated: context.scoped.target_gated || nested.target_gated || !nested.production_active,
    });
    ImplementationOwners {
        legacy: display_type,
        stable: format!("{stable_type} as {trait_name}"),
        implements_trait: true,
    }
}

fn collect_implementation_function(
    function: &syn::ImplItemFn,
    context: CollectionContext<'_>,
    nested: &NestedItemContext,
    owners: &ImplementationOwners,
    output: &mut StructuralAnalysis,
) {
    let local = function.sig.ident.to_string();
    let identity = MetricIdentity {
        name: qualify(context.module_prefix, &format!("{}::{local}", owners.legacy)),
        stable_symbol: stable_qualify(
            &context.scoped.package,
            context.module_prefix,
            &format!("{}::{local}", owners.stable),
        ),
    };
    let body = FunctionBody {
        signature: &function.sig,
        span: function.span(),
        block: &function.block,
        attributes: &function.attrs,
        visibility: implementation_method_visibility(owners.implements_trait, &function.vis),
    };
    collect_metric(identity, body, context, nested, output);
}

fn implementation_method_visibility(implemented_trait: bool, value: &syn::Visibility) -> SymbolVisibility {
    if implemented_trait {
        SymbolVisibility::Unknown
    } else {
        module_visibility(value)
    }
}

fn collect_trait_defaults(
    trait_item: &syn::ItemTrait,
    context: CollectionContext<'_>,
    output: &mut StructuralAnalysis,
) {
    let nested = nested_item_context(&trait_item.attrs, context);
    let functions = trait_item.items.iter().filter_map(|member| match member {
        syn::TraitItem::Fn(function) if context.scoped.cfg.attrs_active(&function.attrs) => Some(function),
        _ => None,
    });
    for function in functions {
        let Some(block) = &function.default else {
            continue;
        };
        collect_trait_function(function, block, trait_item, context, &nested, output);
    }
}

fn collect_trait_function(
    function: &syn::TraitItemFn,
    block: &Block,
    trait_item: &syn::ItemTrait,
    context: CollectionContext<'_>,
    nested: &NestedItemContext,
    output: &mut StructuralAnalysis,
) {
    let local = function.sig.ident.to_string();
    let owner = format!("{}::{local}", trait_item.ident);
    let identity = MetricIdentity {
        name: qualify(context.module_prefix, &owner),
        stable_symbol: stable_qualify(
            &context.scoped.package,
            context.module_prefix,
            &format!("trait {owner}"),
        ),
    };
    let body = FunctionBody {
        signature: &function.sig,
        span: function.span(),
        block,
        attributes: &function.attrs,
        visibility: module_visibility(&trait_item.vis),
    };
    collect_metric(identity, body, context, nested, output);
}

fn collect_inline_module(
    module: &syn::ItemMod,
    context: CollectionContext<'_>,
    output: &mut StructuralAnalysis,
) {
    let Some((_, items)) = &module.content else {
        return;
    };
    let next_prefix = qualify(context.module_prefix, &module.ident.to_string());
    let nested = nested_item_context(&module.attrs, context);
    output.repository.modules.push(ModuleRecord {
        stable_symbol: stable_with_cfg(qualify(&context.scoped.package, &next_prefix), &nested.cfg, &[]),
        file: context.file.to_string(),
        package: Some(context.scoped.package.clone()),
        visibility: module_visibility(&module.vis),
        references: 0,
        target_gated: context.scoped.target_gated || nested.target_gated,
        generated: false,
        framework_managed: !nested.production_active || module_framework_managed(&module.attrs),
        reflection_reachable: module_reflection_reachable(&module.attrs) || items_reflection_reachable(items),
        externally_invoked: false,
    });
    collect_items(
        items,
        CollectionContext {
            module_prefix: &next_prefix,
            inherited_cfg: &nested.cfg,
            inherited_target_gated: nested.target_gated,
            inherited_production_active: nested.production_active,
            ..context
        },
        output,
    );
}

#[derive(Default)]
struct FeatureReferenceVisitor {
    counts: BTreeMap<String, u32>,
}

impl FeatureReferenceVisitor {
    fn collect_meta(&mut self, meta: &Meta) {
        match meta {
            Meta::NameValue(value) => self.collect_name_value(value),
            Meta::List(list) => self.collect_list(list),
            Meta::Path(_) => {}
        }
    }

    fn collect_name_value(&mut self, value: &syn::MetaNameValue) {
        if !value.path.is_ident("feature") {
            return;
        }
        let Some(feature) = feature_literal(&value.value) else {
            return;
        };
        let count = self.counts.entry(feature).or_default();
        *count = count.saturating_add(1);
    }

    fn collect_list(&mut self, list: &syn::MetaList) {
        let Some(items) = parse_meta_list_for_analysis(list.tokens.clone()) else {
            return;
        };
        for item in items {
            self.collect_meta(&item);
        }
    }
}

fn feature_literal(expression: &Expr) -> Option<String> {
    let Expr::Lit(value) = expression else {
        return None;
    };
    let syn::Lit::Str(value) = &value.lit else {
        return None;
    };
    Some(value.value())
}

impl<'ast> Visit<'ast> for FeatureReferenceVisitor {
    fn visit_attribute(&mut self, node: &'ast Attribute) {
        self.collect_meta(&node.meta);
        visit::visit_attribute(self, node);
    }
}

#[derive(Default)]
struct FrameworkCallbackVisitor {
    references: Vec<(String, usize)>,
}

fn consume_unknown_nested_meta(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        return consume_nested_value(meta);
    }
    if meta.input.peek(syn::token::Paren) {
        return consume_parenthesized_meta(meta);
    }
    Ok(())
}

fn consume_nested_value(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    let _ = meta.value()?.parse::<Expr>()?;
    Ok(())
}

fn consume_parenthesized_meta(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    let content;
    syn::parenthesized!(content in meta.input);
    let _ = content.parse::<proc_macro2::TokenStream>()?;
    Ok(())
}

impl<'ast> Visit<'ast> for FrameworkCallbackVisitor {
    fn visit_attribute(&mut self, node: &'ast Attribute) {
        if node
            .path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "serde")
        {
            let offset = node.span().byte_range().start;
            let _ = node.parse_nested_meta(|meta| {
                let Some(key) = meta.path.segments.last().map(|segment| segment.ident.to_string()) else {
                    return Ok(());
                };
                if !matches!(
                    key.as_str(),
                    "default" | "deserialize_with" | "serialize_with" | "skip_serializing_if" | "with"
                ) || !meta.input.peek(syn::Token![=])
                {
                    return consume_unknown_nested_meta(&meta);
                }
                let value = meta.value()?.parse::<syn::LitStr>()?.value();
                if let Ok(path) = syn::parse_str::<syn::Path>(&value) {
                    self.references.extend(
                        path.segments
                            .iter()
                            .map(|segment| (segment.ident.to_string(), offset)),
                    );
                }
                Ok(())
            });
        }
        visit::visit_attribute(self, node);
    }
}

fn range_contains(ranges: &[std::ops::Range<usize>], offset: usize) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= offset && offset < range.end)
}

fn increment(counts: &mut BTreeMap<String, u32>, identifier: &str) {
    let count = counts.entry(identifier.to_string()).or_default();
    *count = count.saturating_add(1);
}

fn append_identifier_counts(
    syntax: &syn::File,
    source: &str,
    file: &str,
    scopes: &[ScopedFile],
    repository: &mut RepositorySemantics,
) {
    let packages = scopes
        .iter()
        .map(|scope| scope.package.clone())
        .collect::<BTreeSet<_>>();
    for package in packages {
        append_package_identifiers(syntax, source, file, scopes, &package, repository);
    }
}

fn append_package_identifiers(
    syntax: &syn::File,
    source: &str,
    file: &str,
    scopes: &[ScopedFile],
    package: &str,
    repository: &mut RepositorySemantics,
) {
    let cfg = package_cfg(scopes, package);
    let inactive = inactive_file_ranges(syntax, &cfg);
    let test_ranges = explicit_test_ranges(syntax, &cfg);
    let mut all = crate::tokens::identifier_counts(source, &inactive);
    let ranges = IdentifierRanges {
        source,
        file,
        inactive: &inactive,
        tests: &test_ranges,
    };
    let mut production = production_identifier_counts(&ranges);
    append_framework_callbacks(syntax, &ranges, &mut all, &mut production);
    push_identifier_records(package, &all, &production, repository);
    push_feature_records(syntax, package, repository);
}

fn package_cfg(scopes: &[ScopedFile], package: &str) -> CfgContext {
    CfgContext::merged(
        scopes
            .iter()
            .filter(|scope| scope.package == package)
            .map(|scope| &scope.cfg),
    )
}

fn explicit_test_ranges(syntax: &syn::File, cfg: &CfgContext) -> Vec<std::ops::Range<usize>> {
    let selection = RealizableCfg {
        cfg,
        selection: CfgSelection::Production,
    };
    attribute_ranges(AttributeRangeRoot::File(syntax), |attributes| {
        attributes_contain(attributes, &["test", "bench"]) || !selection.attrs_active(attributes)
    })
}

struct IdentifierRanges<'a> {
    source: &'a str,
    file: &'a str,
    inactive: &'a [std::ops::Range<usize>],
    tests: &'a [std::ops::Range<usize>],
}

fn production_identifier_counts(ranges: &IdentifierRanges<'_>) -> BTreeMap<String, u32> {
    if Language::Rust.is_test_path(ranges.file) {
        return BTreeMap::new();
    }
    let mut excluded = ranges.inactive.to_vec();
    excluded.extend_from_slice(ranges.tests);
    crate::tokens::identifier_counts(ranges.source, &excluded)
}

fn append_framework_callbacks(
    syntax: &syn::File,
    ranges: &IdentifierRanges<'_>,
    all: &mut BTreeMap<String, u32>,
    production: &mut BTreeMap<String, u32>,
) {
    let mut callbacks = FrameworkCallbackVisitor::default();
    callbacks.visit_file(syntax);
    for (identifier, offset) in callbacks.references {
        if range_contains(ranges.inactive, offset) {
            continue;
        }
        increment(all, &identifier);
        if callback_in_production(ranges.file, ranges.tests, offset) {
            increment(production, &identifier);
        }
    }
}

fn callback_in_production(file: &str, test_ranges: &[std::ops::Range<usize>], offset: usize) -> bool {
    !Language::Rust.is_test_path(file) && !range_contains(test_ranges, offset)
}

fn push_identifier_records(
    package: &str,
    all: &BTreeMap<String, u32>,
    production: &BTreeMap<String, u32>,
    repository: &mut RepositorySemantics,
) {
    let identifiers = all
        .keys()
        .chain(production.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for identifier in identifiers {
        let production_references = production.get(&identifier).copied().unwrap_or(0);
        let test_references = all
            .get(&identifier)
            .copied()
            .unwrap_or(0)
            .saturating_sub(production_references);
        repository.identifiers.push(IdentifierCountRecord {
            identifier,
            package: Some(package.to_string()),
            production_references,
            test_references,
        });
    }
}

fn push_feature_records(syntax: &syn::File, package: &str, repository: &mut RepositorySemantics) {
    let mut features = FeatureReferenceVisitor::default();
    features.visit_file(syntax);
    for (name, references) in features.counts {
        repository.features.push(FeatureRecord {
            package: package.to_string(),
            name,
            references,
            enables: BTreeSet::new(),
            target_gated: true,
        });
    }
}

pub(crate) fn extract(
    syntax: &syn::File,
    source: &str,
    file: &str,
    scopes: &[ScopedFile],
) -> StructuralAnalysis {
    let mut output = StructuralAnalysis::default();
    for scoped in scopes {
        collect_items(
            &syntax.items,
            CollectionContext {
                source,
                file,
                module_prefix: &scoped.module_prefix,
                scoped,
                inherited_cfg: &scoped.cfg_evidence,
                inherited_target_gated: scoped.target_gated || attributes_target_gated(&syntax.attrs),
                inherited_production_active: root_production_active(file, scoped, &syntax.attrs),
            },
            &mut output,
        );
    }
    output.functions.sort_by(reporigor_core::compare_function_records);
    output.functions.dedup_by(|left, right| {
        left.stable_symbol == right.stable_symbol
            && left.file == right.file
            && left.start_line == right.start_line
            && left.end_line == right.end_line
    });
    append_identifier_counts(syntax, source, file, scopes, &mut output.repository);
    output.repository.identifier_counts_reliable = !scopes.is_empty();
    output.repository.trait_inventory_reliable = !scopes.is_empty();
    output.repository.test_inventory_reliable = !scopes.is_empty();
    output.repository.unreachable_inventory_reliable = !scopes.is_empty();
    output.repository.canonicalize();
    output
}

fn root_production_active(file: &str, scoped: &ScopedFile, attributes: &[Attribute]) -> bool {
    !Language::Rust.is_test_path(file)
        && scoped.cfg.has_production_variant()
        && scoped.cfg.attrs_active_in_production(attributes)
        && !attributes_require_test(attributes)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::test_support::required;

    fn scoped(prefix: &str) -> ScopedFile {
        ScopedFile {
            path: PathBuf::from("src/lib.rs"),
            module_prefix: prefix.to_string(),
            package: "fixture".to_string(),
            cfg: CfgContext::synthetic(false),
            target_gated: false,
            cfg_evidence: String::new(),
            visibility: SymbolVisibility::Public,
            framework_managed: false,
            reflection_reachable: false,
        }
    }

    fn analyze(source: &str) -> StructuralAnalysis {
        analyze_scopes(source, &[scoped("")])
    }

    fn analyze_scopes(source: &str, scopes: &[ScopedFile]) -> StructuralAnalysis {
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        extract(&syntax, source, "src/lib.rs", scopes)
    }

    fn analyze_with_tests(source: &str) -> StructuralAnalysis {
        let production = scoped("");
        let mut tests = scoped("");
        tests.cfg = CfgContext::synthetic(true);
        analyze_scopes(source, &[production, tests])
    }

    fn function<'a>(analysis: &'a StructuralAnalysis, suffix: &str) -> &'a FunctionRecord {
        required(&analysis.functions, suffix, |function| {
            function.name.ends_with(suffix)
        })
    }

    fn identifier_record<'a>(analysis: &'a StructuralAnalysis, name: &str) -> &'a IdentifierCountRecord {
        required(&analysis.repository.identifiers, name, |record| {
            record.identifier == name
        })
    }

    fn stable_symbols(analysis: &StructuralAnalysis) -> BTreeSet<&str> {
        let mut symbols = BTreeSet::new();
        for function in &analysis.functions {
            symbols.insert(function.stable_symbol.as_str());
        }
        symbols
    }

    #[test]
    fn adapter_source_keeps_every_function_at_crap_safe_complexity() {
        let source_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let files =
            "cargo_proxy.rs|command.rs|complexity.rs|lib.rs|mutations.rs|scope.rs|syntax.rs|tokens.rs"
                .split('|');
        let mut violations = Vec::new();
        for file in files {
            let path = source_dir.join(file);
            let source =
                fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let syntax =
                syn::parse_file(&source).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            let analysis = extract(&syntax, &source, &format!("src/{file}"), &[scoped("")]);
            violations.extend(
                analysis
                    .functions
                    .into_iter()
                    .filter(|function| function.complexity > 6)
                    .map(|function| {
                        format!(
                            "{}:{} complexity {}",
                            function.file, function.stable_symbol, function.complexity
                        )
                    }),
            );
        }

        assert!(violations.is_empty(), "{}", violations.join("\n"));
    }

    #[test]
    fn structural_metrics_tokens_and_unreachable_are_ast_owned() {
        let source = r"
fn sample(first: i32, second: i32) -> i32 {
    let local = 42;
    if first > second { return local; let unreachable = 9; }
    external::finish(second)
}
";
        let analysis = analyze_scopes(source, &[scoped("module")]);
        let metric = &analysis.functions[0];
        assert_eq!(metric.stable_symbol, "fixture::module::sample");
        assert_eq!(metric.parameter_count, 2);
        assert_eq!(metric.nesting_depth, 1);
        assert!(metric.statement_count >= 5);
        assert!(metric.normalized_tokens.iter().any(|token| token == "LOCAL"));
        assert!(metric.normalized_tokens.iter().any(|token| token == "LITERAL"));
        assert!(metric.normalized_tokens.iter().any(|token| token == "external"));
        assert_eq!(analysis.repository.unreachable.len(), 1);
        assert!(!analysis.repository.unreachable[0]
            .structural_evidence
            .contains("unreachable = 9"));
    }

    #[test]
    fn match_complexity_counts_alternatives_minus_one_including_default() {
        let source = r"
fn classify(value: i32) -> i32 {
    match value {
        0 => 10,
        1 if value > -1 => 20,
        _ => 30,
    }
}
";
        let analysis = analyze(source);
        let metric = &analysis.functions[0];
        // Base path + two additional alternatives + the guard decision.
        assert_eq!(metric.complexity, 4);
        assert_eq!(metric.nesting_depth, 1);
    }

    #[test]
    fn trait_implementations_and_same_named_methods_have_distinct_symbols() {
        let source = r"
trait Left { fn same(&self); }
trait Right { fn same(&self); }
struct Value;
impl Left for Value { fn same(&self) {} }
impl Right for Value { fn same(&self) {} }
";
        let analysis = analyze(source);
        let symbols = analysis
            .functions
            .iter()
            .map(|function| function.stable_symbol.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(symbols.len(), 2);
        assert!(symbols
            .iter()
            .any(|symbol| symbol.contains("Value as fixture::Left::same")));
        assert!(symbols
            .iter()
            .any(|symbol| symbol.contains("Value as fixture::Right::same")));
        assert_eq!(analysis.repository.trait_implementations.len(), 2);
    }

    #[test]
    fn same_named_local_traits_have_qualified_identities() {
        let source = r"
mod left {
    trait Contract { fn run(&self); }
    struct Value;
    impl Contract for Value { fn run(&self) {} }
}
mod right {
    trait Contract { fn run(&self); }
    struct Value;
    impl Contract for Value { fn run(&self) {} }
}
";
        let analysis = analyze(source);
        let traits = analysis
            .repository
            .trait_implementations
            .iter()
            .map(|implementation| implementation.trait_symbol.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            traits,
            BTreeSet::from(["fixture::left::Contract", "fixture::right::Contract"])
        );
    }

    #[test]
    fn trait_paths_are_stable_across_rust_relative_roots() {
        let locals = BTreeSet::from(["Local".to_string()]);
        let cases = [
            (syn::parse_quote!(Local), "fixture::outer::inner::Local"),
            (syn::parse_quote!(crate::Contract), "fixture::Contract"),
            (
                syn::parse_quote!(self::Contract),
                "fixture::outer::inner::Contract",
            ),
            (syn::parse_quote!(super::Contract), "fixture::outer::Contract"),
            (syn::parse_quote!(super::super::Contract), "fixture::Contract"),
            (syn::parse_quote!(external::Contract), "external::Contract"),
        ];
        for (path, expected) in cases {
            assert_eq!(
                stable_trait_symbol("fixture", "outer::inner", &path, &locals),
                expected
            );
        }

        let empty = syn::Path {
            leading_colon: None,
            segments: syn::punctuated::Punctuated::new(),
        };
        assert!(trait_path_root(&path_segments(&empty)).is_none());
    }

    #[test]
    fn stable_symbols_ignore_unrelated_line_movement_and_cfg_variants_do_not_collide() {
        let before = "#[cfg(unix)] fn launch() {}\n#[cfg(windows)] fn launch() {}\n";
        let after = "\n\n#[cfg(unix)] fn launch() {}\n\n#[cfg(windows)] fn launch() {}\n";
        let mut scope = scoped("");
        scope.cfg = CfgContext::with_synthetic_names(false, &["unix", "windows"]);
        let first = analyze_scopes(before, std::slice::from_ref(&scope));
        let second = analyze_scopes(after, &[scope]);
        let first_symbols = stable_symbols(&first);
        let second_symbols = stable_symbols(&second);
        assert_eq!(first_symbols, second_symbols);
        assert_eq!(first_symbols.len(), 2);
        assert!(first.functions.iter().all(|function| function.entry_point));
    }

    #[test]
    fn method_references_include_shared_fields() {
        let source = r"
struct Account { balance: i32 }
impl Account {
    fn debit(&mut self) { let balance = 1; self.balance -= balance; }
    fn credit(&mut self) { let balance = 1; self.balance += balance; }
}
";
        let analysis = analyze(source);
        let methods: Vec<_> = analysis
            .functions
            .iter()
            .filter(|function| function.stable_symbol.contains("Account::"))
            .collect();
        assert_eq!(methods.len(), 2);
        assert!(methods
            .iter()
            .all(|function| function.references.contains("field::balance")));
        assert!(methods
            .iter()
            .all(|function| !function.references.contains("balance")));
    }

    #[test]
    fn method_references_survive_same_named_local_bindings() {
        let source = r"
struct Worker;
impl Worker {
    fn helper(&self) {}
    fn caller(&self) {
        let helper = 1;
        self.helper();
        consume(helper);
    }
}
";
        let analysis = analyze(source);
        let caller = function(&analysis, "Worker::caller");
        assert!(caller.references.contains("method::helper"));
        assert!(!caller.references.contains("helper"));
    }

    #[test]
    fn framework_callback_strings_are_production_references() {
        let source = r#"
fn default_manifest_file_name() -> String { String::new() }
#[derive(serde::Deserialize)]
struct Settings {
    #[serde(rename = "manifestFile", default = "default_manifest_file_name")]
    manifest: String,
}
"#;
        let analysis = analyze(source);
        let callback = identifier_record(&analysis, "default_manifest_file_name");
        assert!(callback.production_references >= 2);
    }

    #[test]
    fn production_identifier_counts_respect_test_or_feature_cfg() {
        let source = r#"
#[cfg(any(test, feature = "x"))]
fn conditional() { hidden_reference(); }
fn local_callback() {}
fn arm_callback() {}
fn macro_callback() {}
fn expression_callback() {}
fn production() {
    #[cfg(test)]
    let _local = local_callback();
    match 0 {
        #[cfg(test)]
        0 => arm_callback(),
        _ => (),
    }
    #[cfg(test)]
    println!("{}", macro_callback());
    #[cfg(test)]
    expression_callback();
}
"#;
        let disabled = analyze_with_tests(source);
        let disabled_reference = identifier_record(&disabled, "hidden_reference");
        assert_eq!(disabled_reference.production_references, 0);
        assert!(disabled_reference.test_references > 0);
        for identifier in [
            "local_callback",
            "arm_callback",
            "macro_callback",
            "expression_callback",
        ] {
            let reference = identifier_record(&disabled, identifier);
            assert_eq!(reference.production_references, 1, "{identifier}");
            assert_eq!(reference.test_references, 1, "{identifier}");
        }

        let mut production = scoped("");
        production.cfg = CfgContext::with_synthetic_features(false, &["x"]);
        let mut tests = scoped("");
        tests.cfg = CfgContext::with_synthetic_features(true, &["x"]);
        let enabled = analyze_scopes(source, &[production, tests]);
        let enabled_reference = identifier_record(&enabled, "hidden_reference");
        assert!(enabled_reference.production_references > 0);
    }

    #[test]
    fn merged_test_context_does_not_combine_mutually_exclusive_function_variants() {
        let source = r"
fn selected_variant(
    #[cfg(not(test))] production_argument: i32,
    #[cfg(test)] test_argument: i32,
) {
    #[cfg(not(test))]
    if production_argument > 0 { production_work(); }
    #[cfg(test)]
    if test_argument > 0 {
        if test_condition() { test_work(); }
    }
}
#[cfg(test)]
fn test_variant() {
    #[cfg(test)]
    if test_condition() { test_work(); }
    #[cfg(not(test))]
    if production_condition() { production_work(); }
}
";
        let production = analyze(source);

        let production_cfg = CfgContext::synthetic(false);
        let tests_cfg = CfgContext::synthetic(true);
        let mut merged = scoped("");
        merged.cfg = CfgContext::merged([&production_cfg, &tests_cfg]);
        let with_tests = analyze_scopes(source, &[merged]);

        let production_only = function(&production, "selected_variant");
        let merged_production = function(&with_tests, "selected_variant");
        assert_eq!(merged_production.complexity, production_only.complexity);
        assert_eq!(merged_production.nesting_depth, production_only.nesting_depth);
        assert_eq!(merged_production.statement_count, production_only.statement_count);
        assert_eq!(merged_production.parameter_count, 1);
        assert_eq!(
            merged_production.normalized_tokens,
            production_only.normalized_tokens
        );
        assert_eq!(merged_production.references, production_only.references);

        let test_only = function(&with_tests, "test_variant");
        assert!(test_only
            .normalized_tokens
            .iter()
            .any(|token| token == "test_work"));
        assert!(!test_only
            .normalized_tokens
            .iter()
            .any(|token| token == "production_work"));
        assert!(test_only.references.contains("test_work"));
        assert!(!test_only.references.contains("production_work"));
    }

    #[test]
    fn variant_gated_statements_are_not_reported_as_unreachable() {
        let source = r"
fn selected_variant() {
    #[cfg(test)]
    return;
    #[cfg(not(test))]
    production_work();
}
fn truly_unreachable() { return; production_work(); }
";
        let analysis = analyze_with_tests(source);
        assert!(analysis
            .repository
            .unreachable
            .iter()
            .filter(|record| record.stable_symbol.ends_with("selected_variant"))
            .all(|record| record.target_gated));
        assert!(analysis
            .repository
            .unreachable
            .iter()
            .any(|record| record.stable_symbol.ends_with("truly_unreachable") && !record.target_gated));
    }

    #[test]
    fn outer_normalized_tokens_exclude_nested_executable_bodies() {
        let first = r"
fn outer() {
    fn first_nested(first: i32) -> i32 { first_body(first) }
    let callback = |shared_work: i32| first_closure(shared_work);
    shared_work();
}
";
        let second = r"
fn outer() {
    fn second_nested(second: &str, flag: bool) -> usize { second_body(second, flag) }
    let callback = move |renamed: i32, other: i32| second_closure(renamed, other);
    shared_work();
}
";
        let tokens = |source: &str| function(&analyze(source), "outer").normalized_tokens.clone();
        let first_tokens = tokens(first);
        let second_tokens = tokens(second);
        assert_eq!(first_tokens, second_tokens);
        assert!(first_tokens.iter().any(|token| token == "shared_work"));
    }

    #[test]
    fn same_line_nested_body_records_an_ambiguous_coverage_boundary() {
        let source = "fn outer() { let callback = || nested(); outer_work(); }\n";
        let analysis = analyze(source);
        let outer = function(&analysis, "outer");
        assert!(outer
            .coverage_excluded_ranges
            .iter()
            .any(|(start, end)| start == end));
    }
}
