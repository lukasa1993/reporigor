use std::ops::Range;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ForeignItem, ImplItem, Item, TraitItem};

use crate::scope::CfgContext;

pub(crate) fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(syn::ItemConst { attrs, .. })
        | Item::Enum(syn::ItemEnum { attrs, .. })
        | Item::ExternCrate(syn::ItemExternCrate { attrs, .. })
        | Item::Fn(syn::ItemFn { attrs, .. })
        | Item::ForeignMod(syn::ItemForeignMod { attrs, .. })
        | Item::Impl(syn::ItemImpl { attrs, .. })
        | Item::Macro(syn::ItemMacro { attrs, .. })
        | Item::Mod(syn::ItemMod { attrs, .. })
        | Item::Static(syn::ItemStatic { attrs, .. })
        | Item::Struct(syn::ItemStruct { attrs, .. })
        | Item::Trait(syn::ItemTrait { attrs, .. })
        | Item::TraitAlias(syn::ItemTraitAlias { attrs, .. })
        | Item::Type(syn::ItemType { attrs, .. })
        | Item::Union(syn::ItemUnion { attrs, .. })
        | Item::Use(syn::ItemUse { attrs, .. }) => attrs,
        _ => empty_attributes(),
    }
}

pub(crate) fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(syn::ImplItemConst { attrs, .. })
        | ImplItem::Fn(syn::ImplItemFn { attrs, .. })
        | ImplItem::Type(syn::ImplItemType { attrs, .. })
        | ImplItem::Macro(syn::ImplItemMacro { attrs, .. }) => attrs,
        _ => empty_attributes(),
    }
}

pub(crate) fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(syn::TraitItemConst { attrs, .. })
        | TraitItem::Fn(syn::TraitItemFn { attrs, .. })
        | TraitItem::Type(syn::TraitItemType { attrs, .. })
        | TraitItem::Macro(syn::TraitItemMacro { attrs, .. }) => attrs,
        _ => empty_attributes(),
    }
}

pub(crate) fn foreign_item_attrs(item: &ForeignItem) -> &[Attribute] {
    match item {
        ForeignItem::Fn(syn::ForeignItemFn { attrs, .. })
        | ForeignItem::Static(syn::ForeignItemStatic { attrs, .. })
        | ForeignItem::Type(syn::ForeignItemType { attrs, .. })
        | ForeignItem::Macro(syn::ForeignItemMacro { attrs, .. }) => attrs,
        _ => empty_attributes(),
    }
}

pub(crate) fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(syn::ExprArray { attrs, .. })
        | Expr::Assign(syn::ExprAssign { attrs, .. })
        | Expr::Async(syn::ExprAsync { attrs, .. })
        | Expr::Await(syn::ExprAwait { attrs, .. })
        | Expr::Binary(syn::ExprBinary { attrs, .. })
        | Expr::Block(syn::ExprBlock { attrs, .. })
        | Expr::Break(syn::ExprBreak { attrs, .. })
        | Expr::Call(syn::ExprCall { attrs, .. })
        | Expr::Cast(syn::ExprCast { attrs, .. })
        | Expr::Closure(syn::ExprClosure { attrs, .. })
        | Expr::Const(syn::ExprConst { attrs, .. })
        | Expr::Continue(syn::ExprContinue { attrs, .. })
        | Expr::Field(syn::ExprField { attrs, .. })
        | Expr::ForLoop(syn::ExprForLoop { attrs, .. })
        | Expr::Group(syn::ExprGroup { attrs, .. })
        | Expr::If(syn::ExprIf { attrs, .. })
        | Expr::Index(syn::ExprIndex { attrs, .. })
        | Expr::Infer(syn::ExprInfer { attrs, .. })
        | Expr::Let(syn::ExprLet { attrs, .. })
        | Expr::Lit(syn::ExprLit { attrs, .. })
        | Expr::Loop(syn::ExprLoop { attrs, .. })
        | Expr::Macro(syn::ExprMacro { attrs, .. })
        | Expr::Match(syn::ExprMatch { attrs, .. })
        | Expr::MethodCall(syn::ExprMethodCall { attrs, .. })
        | Expr::Paren(syn::ExprParen { attrs, .. })
        | Expr::Path(syn::ExprPath { attrs, .. })
        | Expr::Range(syn::ExprRange { attrs, .. })
        | Expr::RawAddr(syn::ExprRawAddr { attrs, .. })
        | Expr::Reference(syn::ExprReference { attrs, .. })
        | Expr::Repeat(syn::ExprRepeat { attrs, .. })
        | Expr::Return(syn::ExprReturn { attrs, .. })
        | Expr::Struct(syn::ExprStruct { attrs, .. })
        | Expr::Try(syn::ExprTry { attrs, .. })
        | Expr::TryBlock(syn::ExprTryBlock { attrs, .. })
        | Expr::Tuple(syn::ExprTuple { attrs, .. })
        | Expr::Unary(syn::ExprUnary { attrs, .. })
        | Expr::Unsafe(syn::ExprUnsafe { attrs, .. })
        | Expr::While(syn::ExprWhile { attrs, .. })
        | Expr::Yield(syn::ExprYield { attrs, .. }) => attrs,
        _ => empty_attributes(),
    }
}

pub(crate) fn attributes_contain(attributes: &[Attribute], names: &[&str]) -> bool {
    attributes
        .iter()
        .any(|attribute| names.iter().any(|name| attribute.path().is_ident(name)))
}

fn empty_attributes() -> &'static [Attribute] {
    &[]
}

fn range_with_attrs(attrs: &[Attribute], node: &impl Spanned) -> Range<usize> {
    let range = node.span().byte_range();
    let start = attrs
        .first()
        .map_or(range.start, |attribute| attribute.span().byte_range().start);
    start..range.end
}

struct AttributeRangeVisitor<F> {
    excluded: F,
    ranges: Vec<Range<usize>>,
}

impl<F> AttributeRangeVisitor<F>
where
    F: FnMut(&[Attribute]) -> bool,
{
    fn inactive(&mut self, attrs: &[Attribute], node: &impl Spanned) -> bool {
        if (self.excluded)(attrs) {
            self.ranges.push(range_with_attrs(attrs, node));
            true
        } else {
            false
        }
    }
}

impl<'ast, F> Visit<'ast> for AttributeRangeVisitor<F>
where
    F: FnMut(&[Attribute]) -> bool,
{
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

#[derive(Clone, Copy)]
pub(crate) enum AttributeRangeRoot<'a> {
    Block(&'a syn::Block),
    File(&'a syn::File),
}

pub(crate) fn attribute_ranges(
    root: AttributeRangeRoot<'_>,
    excluded: impl FnMut(&[Attribute]) -> bool,
) -> Vec<Range<usize>> {
    let mut visitor = AttributeRangeVisitor {
        excluded,
        ranges: Vec::new(),
    };
    match root {
        AttributeRangeRoot::Block(block) => visitor.visit_block(block),
        AttributeRangeRoot::File(file) => visitor.visit_file(file),
    }
    visitor.ranges
}

pub(crate) fn inactive_file_ranges(file: &syn::File, cfg: &CfgContext) -> Vec<Range<usize>> {
    let mut ranges = attribute_ranges(AttributeRangeRoot::File(file), |attrs| !cfg.attrs_active(attrs));
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges
}
