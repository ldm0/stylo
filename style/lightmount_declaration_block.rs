//! Lightmount-facing CSS declaration block hooks.

use std::borrow::Cow;

use cssparser::{Parser, ParserInput};
use style_traits::{CssString, CssWriter, ParsingMode, ToCss};

use crate::{
    context::QuirksMode,
    custom_properties::AttrTaint,
    parser::ParserContext,
    properties::{
        parse_property_declaration_list, Importance, PropertyDeclarationBlock, PropertyId,
    },
    stylesheets::{CssRuleType, Namespaces, Origin, UrlExtraData},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssDeclarationEntry {
    pub name: String,
    pub value: String,
    pub priority: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CssDeclarationBlock {
    block: PropertyDeclarationBlock,
}

impl CssDeclarationBlock {
    pub fn new(block: PropertyDeclarationBlock) -> Self {
        Self { block }
    }

    pub fn is_empty(&self) -> bool {
        self.block.is_empty()
    }

    pub fn len(&self) -> usize {
        self.block.len()
    }

    pub fn css_text(&self) -> String {
        let mut output = CssString::new();
        self.block.to_css(&mut output).ok();
        output
    }

    pub fn entries(&self) -> Vec<CssDeclarationEntry> {
        self.block
            .declaration_importance_iter()
            .filter_map(|(declaration, importance)| {
                let mut name = String::new();
                declaration
                    .id()
                    .to_css(&mut CssWriter::new(&mut name))
                    .ok()?;
                let mut value = CssString::new();
                declaration.to_css(&mut value).ok()?;
                Some(CssDeclarationEntry {
                    name,
                    value,
                    priority: importance.important(),
                })
            })
            .collect()
    }

    pub fn property_value(&self, name: &str) -> Option<String> {
        let property = PropertyId::parse_enabled_for_all_content(name).ok()?;
        let mut output = CssString::new();
        self.block
            .property_value_to_css(&property, &mut output)
            .ok()?;
        Some(output)
    }

    pub fn property_priority(&self, name: &str) -> bool {
        let Ok(property) = PropertyId::parse_enabled_for_all_content(name) else {
            return false;
        };
        self.block.property_priority(&property) == Importance::Important
    }

    pub fn into_inner(self) -> PropertyDeclarationBlock {
        self.block
    }
}

pub fn parse_declaration_block(css_text: &str) -> CssDeclarationBlock {
    with_declaration_context(|context| {
        let mut input = ParserInput::new(css_text);
        let mut parser = Parser::new(&mut input);
        CssDeclarationBlock::new(parse_property_declaration_list(context, &mut parser, &[]))
    })
    .unwrap_or_default()
}

fn with_declaration_context<R>(f: impl FnOnce(&ParserContext) -> R) -> Option<R> {
    let url_data = UrlExtraData::from(url::Url::parse("about:blank").ok()?);
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(CssRuleType::Style),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    Some(f(&context))
}

#[cfg(test)]
mod tests {
    use super::parse_declaration_block;

    #[test]
    fn declaration_block_uses_stylo_parser_and_cssom_serialization() {
        let block = parse_declaration_block(
            "width: 0; color: invalid; margin: 1px 2px; --token: a b; color: red !important;",
        );

        assert_eq!(block.property_value("width").as_deref(), Some("0px"));
        assert_eq!(block.property_value("margin").as_deref(), Some("1px 2px"));
        assert_eq!(block.property_value("--token").as_deref(), Some("a b"));
        assert_eq!(block.property_value("color").as_deref(), Some("red"));
        assert!(block.property_priority("color"));
        assert_eq!(
            block.css_text(),
            "width: 0px; margin: 1px 2px; --token: a b; color: red !important;"
        );
    }

    #[test]
    fn declaration_block_entries_are_expanded_pdb_declarations() {
        let block = parse_declaration_block("padding: 1px 2px; background-color: transparent;");
        let entries = block.entries();

        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].name, "padding-top");
        assert_eq!(entries[0].value, "1px");
        assert_eq!(entries[1].name, "padding-right");
        assert_eq!(entries[1].value, "2px");
        assert_eq!(entries[4].name, "background-color");
        assert_eq!(entries[4].value, "transparent");
    }
}
