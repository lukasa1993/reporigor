use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{BinOp, Block, ExprBinary};

use reporigor_core::{FunctionRecord, Language};

use crate::scope::ScopedFile;
use crate::syntax::expr_attrs;

struct ComplexityVisitor<'a> {
    value: u32,
    scoped: &'a ScopedFile,
}

impl<'a> ComplexityVisitor<'a> {
    fn for_block(block: &Block, scoped: &'a ScopedFile) -> u32 {
        let mut visitor = Self { value: 1, scoped };
        visitor.visit_block(block);
        visitor.value
    }
}

impl<'ast> Visit<'ast> for ComplexityVisitor<'_> {
    fn visit_item(&mut self, _node: &'ast syn::Item) {
        // A block-level item declares a nested function/type; its decisions do
        // not execute as part of the enclosing function invocation.
    }

    fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {
        // Anonymous closure bodies have no stable cross-backend function name
        // and are deliberately outside the shared function metric domain. Do
        // not charge their independently executed decisions to their owner.
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        if self.scoped.cfg.attrs_active(expr_attrs(node)) {
            visit::visit_expr(self, node);
        }
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.value = self.value.saturating_add(1);
        visit::visit_expr_if(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.value = self.value.saturating_add(1);
        visit::visit_expr_for_loop(self, node);
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.value = self.value.saturating_add(1);
        visit::visit_expr_while(self, node);
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.value = self.value.saturating_add(1);
        visit::visit_expr_loop(self, node);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if !self.scoped.cfg.attrs_active(&node.attrs) {
            return;
        }
        self.value = self.value.saturating_add(1);
        visit::visit_arm(self, node);
    }

    fn visit_pat_guard(&mut self, node: &'ast syn::PatGuard) {
        self.value = self.value.saturating_add(1);
        visit::visit_pat_guard(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        if matches!(node.op, BinOp::And(_) | BinOp::Or(_)) {
            self.value = self.value.saturating_add(1);
        }
        visit::visit_expr_binary(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if !self.scoped.cfg.attrs_active(&node.attrs) {
            return;
        }
        if node.init.as_ref().is_some_and(|init| init.diverge.is_some()) {
            self.value = self.value.saturating_add(1);
        }
        visit::visit_local(self, node);
    }

    fn visit_expr_try(&mut self, node: &'ast syn::ExprTry) {
        self.value = self.value.saturating_add(1);
        visit::visit_expr_try(self, node);
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
    } else {
        format!("{prefix}::{local}")
    }
}

fn line(value: usize) -> u32 {
    u32::try_from(value.max(1)).unwrap_or(u32::MAX)
}

fn metric(name: String, file: &str, span: Span, block: &Block, scoped: &ScopedFile) -> FunctionRecord {
    let start = span.start();
    let end = span.end();
    FunctionRecord {
        language: Language::Rust,
        name,
        file: file.to_string(),
        start_line: line(start.line),
        end_line: line(end.line.max(start.line)),
        complexity: ComplexityVisitor::for_block(block, scoped),
        coverage: None,
        crap: None,
    }
}

fn collect_items(
    items: &[syn::Item],
    source: &str,
    file: &str,
    module_prefix: &str,
    scoped: &ScopedFile,
    output: &mut Vec<FunctionRecord>,
) {
    for item in items {
        match item {
            syn::Item::Fn(function) => {
                if !scoped.cfg.attrs_active(&function.attrs) {
                    continue;
                }
                output.push(metric(
                    qualify(module_prefix, &function.sig.ident.to_string()),
                    file,
                    function.span(),
                    &function.block,
                    scoped,
                ));
            }
            syn::Item::Impl(implementation) => {
                if !scoped.cfg.attrs_active(&implementation.attrs) {
                    continue;
                }
                let owner = slice_span(source, implementation.self_ty.span());
                for member in &implementation.items {
                    if let syn::ImplItem::Fn(function) = member {
                        if !scoped.cfg.attrs_active(&function.attrs) {
                            continue;
                        }
                        output.push(metric(
                            qualify(module_prefix, &format!("{owner}::{}", function.sig.ident)),
                            file,
                            function.span(),
                            &function.block,
                            scoped,
                        ));
                    }
                }
            }
            syn::Item::Trait(trait_item) => {
                if !scoped.cfg.attrs_active(&trait_item.attrs) {
                    continue;
                }
                let owner = trait_item.ident.to_string();
                for member in &trait_item.items {
                    if let syn::TraitItem::Fn(function) = member {
                        if !scoped.cfg.attrs_active(&function.attrs) {
                            continue;
                        }
                        if let Some(block) = &function.default {
                            output.push(metric(
                                qualify(module_prefix, &format!("{owner}::{}", function.sig.ident)),
                                file,
                                function.span(),
                                block,
                                scoped,
                            ));
                        }
                    }
                }
            }
            syn::Item::Mod(module) => {
                if !scoped.cfg.attrs_active(&module.attrs) {
                    continue;
                }
                if let Some((_, nested)) = &module.content {
                    collect_items(
                        nested,
                        source,
                        file,
                        &qualify(module_prefix, &module.ident.to_string()),
                        scoped,
                        output,
                    );
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn extract(
    syntax: &syn::File,
    source: &str,
    file: &str,
    scopes: &[ScopedFile],
) -> Vec<FunctionRecord> {
    let mut output = Vec::new();
    for scoped in scopes {
        collect_items(
            &syntax.items,
            source,
            file,
            &scoped.module_prefix,
            scoped,
            &mut output,
        );
    }
    output.sort_by(|left, right| (left.start_line, &left.name).cmp(&(right.start_line, &right.name)));
    output.dedup_by(|left, right| {
        left.name == right.name
            && left.file == right.file
            && left.start_line == right.start_line
            && left.end_line == right.end_line
    });
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::CfgContext;

    fn scoped(prefix: &str) -> ScopedFile {
        ScopedFile {
            path: "sample.rs".into(),
            module_prefix: prefix.into(),
            cfg: CfgContext::synthetic(false),
        }
    }

    #[test]
    fn extracts_qualified_functions_and_rust_complexity() {
        let source = r"
pub fn choose(a: bool, b: bool) -> Result<i32, ()> {
    if a && b { Ok(1) } else { Err(())? }
}
struct Thing;
impl Thing {
    fn value(&self, x: i32) -> i32 { match x { 0 => 1, _ => 2 } }
}
";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let metrics = extract(&syntax, source, "src/logic.rs", &[scoped("logic")]);
        let choose = metrics
            .iter()
            .find(|item| item.name == "logic::choose")
            .unwrap_or_else(|| panic!("missing free function: {metrics:?}"));
        assert!(choose.complexity >= 4);
        assert!(metrics.iter().any(|item| item.name == "logic::Thing::value"));
    }

    #[test]
    fn inactive_expression_does_not_add_complexity() {
        let source = r"
fn choose(x: bool) -> i32 {
    #[cfg(any())]
    if x { return 1; }
    0
}
";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let metrics = extract(&syntax, source, "src/lib.rs", &[scoped("")]);
        assert_eq!(metrics[0].complexity, 1);
    }

    #[test]
    fn nested_functions_and_closures_do_not_inflate_outer_complexity() {
        let source = r"
fn outer(flag: bool) {
    if flag { work(); }
    fn nested(value: bool) {
        if value { loop {} }
    }
    let deferred = || {
        if flag { while flag { break; } }
    };
}
";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let metrics = extract(&syntax, source, "src/lib.rs", &[scoped("")]);
        let outer = metrics
            .iter()
            .find(|item| item.name == "outer")
            .unwrap_or_else(|| panic!("missing outer function: {metrics:?}"));
        assert_eq!(metrics.len(), 1, "nested executable leaked: {metrics:?}");
        assert_eq!(outer.complexity, 2);
        assert!(!metrics.iter().any(|item| item.name.contains("nested")));
    }
}
