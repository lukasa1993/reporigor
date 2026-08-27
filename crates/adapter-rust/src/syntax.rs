use std::ops::Range;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ForeignItem, ImplItem, Item, TraitItem};

use crate::scope::CfgContext;

pub(crate) fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(value) => &value.attrs,
        Item::Enum(value) => &value.attrs,
        Item::ExternCrate(value) => &value.attrs,
        Item::Fn(value) => &value.attrs,
        Item::ForeignMod(value) => &value.attrs,
        Item::Impl(value) => &value.attrs,
        Item::Macro(value) => &value.attrs,
        Item::Mod(value) => &value.attrs,
        Item::Static(value) => &value.attrs,
        Item::Struct(value) => &value.attrs,
        Item::Trait(value) => &value.attrs,
        Item::TraitAlias(value) => &value.attrs,
        Item::Type(value) => &value.attrs,
        Item::Union(value) => &value.attrs,
        Item::Use(value) => &value.attrs,
        _ => &[],
    }
}

pub(crate) fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(value) => &value.attrs,
        ImplItem::Fn(value) => &value.attrs,
        ImplItem::Type(value) => &value.attrs,
        ImplItem::Macro(value) => &value.attrs,
        _ => &[],
    }
}

pub(crate) fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(value) => &value.attrs,
        TraitItem::Fn(value) => &value.attrs,
        TraitItem::Type(value) => &value.attrs,
        TraitItem::Macro(value) => &value.attrs,
        _ => &[],
    }
}

pub(crate) fn foreign_item_attrs(item: &ForeignItem) -> &[Attribute] {
    match item {
        ForeignItem::Fn(value) => &value.attrs,
        ForeignItem::Static(value) => &value.attrs,
        ForeignItem::Type(value) => &value.attrs,
        ForeignItem::Macro(value) => &value.attrs,
        _ => &[],
    }
}

pub(crate) fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(value) => &value.attrs,
        Expr::Assign(value) => &value.attrs,
        Expr::Async(value) => &value.attrs,
        Expr::Await(value) => &value.attrs,
        Expr::Binary(value) => &value.attrs,
        Expr::Block(value) => &value.attrs,
        Expr::Break(value) => &value.attrs,
        Expr::Call(value) => &value.attrs,
        Expr::Cast(value) => &value.attrs,
        Expr::Closure(value) => &value.attrs,
        Expr::Const(value) => &value.attrs,
        Expr::Continue(value) => &value.attrs,
        Expr::Field(value) => &value.attrs,
        Expr::ForLoop(value) => &value.attrs,
        Expr::Group(value) => &value.attrs,
        Expr::If(value) => &value.attrs,
        Expr::Index(value) => &value.attrs,
        Expr::Infer(value) => &value.attrs,
        Expr::Let(value) => &value.attrs,
        Expr::Lit(value) => &value.attrs,
        Expr::Loop(value) => &value.attrs,
        Expr::Macro(value) => &value.attrs,
        Expr::Match(value) => &value.attrs,
        Expr::MethodCall(value) => &value.attrs,
        Expr::Paren(value) => &value.attrs,
        Expr::Path(value) => &value.attrs,
        Expr::Range(value) => &value.attrs,
        Expr::RawAddr(value) => &value.attrs,
        Expr::Reference(value) => &value.attrs,
        Expr::Repeat(value) => &value.attrs,
        Expr::Return(value) => &value.attrs,
        Expr::Struct(value) => &value.attrs,
        Expr::Try(value) => &value.attrs,
        Expr::TryBlock(value) => &value.attrs,
        Expr::Tuple(value) => &value.attrs,
        Expr::Unary(value) => &value.attrs,
        Expr::Unsafe(value) => &value.attrs,
        Expr::While(value) => &value.attrs,
        Expr::Yield(value) => &value.attrs,
        _ => &[],
    }
}

fn range_with_attrs(attrs: &[Attribute], node: &impl Spanned) -> Range<usize> {
    let range = node.span().byte_range();
    let start = attrs
        .first()
        .map_or(range.start, |attribute| attribute.span().byte_range().start);
    start..range.end
}

struct InactiveRangeVisitor<'a> {
    cfg: &'a CfgContext,
    ranges: Vec<Range<usize>>,
}

impl InactiveRangeVisitor<'_> {
    fn inactive(&mut self, attrs: &[Attribute], node: &impl Spanned) -> bool {
        if self.cfg.attrs_active(attrs) {
            false
        } else {
            self.ranges.push(range_with_attrs(attrs, node));
            true
        }
    }
}

impl<'ast> Visit<'ast> for InactiveRangeVisitor<'_> {
    fn visit_item(&mut self, node: &'ast Item) {
        if !self.inactive(item_attrs(node), node) {
            visit::visit_item(self, node);
        }
    }

    fn visit_impl_item(&mut self, node: &'ast ImplItem) {
        if !self.inactive(impl_item_attrs(node), node) {
            visit::visit_impl_item(self, node);
        }
    }

    fn visit_trait_item(&mut self, node: &'ast TraitItem) {
        if !self.inactive(trait_item_attrs(node), node) {
            visit::visit_trait_item(self, node);
        }
    }

    fn visit_foreign_item(&mut self, node: &'ast ForeignItem) {
        if !self.inactive(foreign_item_attrs(node), node) {
            visit::visit_foreign_item(self, node);
        }
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if !self.inactive(&node.attrs, node) {
            visit::visit_local(self, node);
        }
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if !self.inactive(&node.attrs, node) {
            visit::visit_arm(self, node);
        }
    }

    fn visit_field(&mut self, node: &'ast syn::Field) {
        if !self.inactive(&node.attrs, node) {
            visit::visit_field(self, node);
        }
    }

    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if !self.inactive(&node.attrs, node) {
            visit::visit_field_value(self, node);
        }
    }

    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if !self.inactive(&node.attrs, node) {
            visit::visit_variant(self, node);
        }
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        if !self.inactive(&node.attrs, node) {
            visit::visit_stmt_macro(self, node);
        }
    }

    fn visit_expr(&mut self, node: &'ast Expr) {
        if !self.inactive(expr_attrs(node), node) {
            visit::visit_expr(self, node);
        }
    }
}

pub(crate) fn inactive_file_ranges(file: &syn::File, cfg: &CfgContext) -> Vec<Range<usize>> {
    let mut visitor = InactiveRangeVisitor {
        cfg,
        ranges: Vec::new(),
    };
    visitor.visit_file(file);
    visitor.ranges.sort_by_key(|range| (range.start, range.end));
    visitor.ranges
}
