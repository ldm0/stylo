//! Lightmount-facing CSS declaration block hooks.

use std::borrow::Cow;

use cssparser::{Parser, ParserInput};
use style_traits::{CssString, CssWriter, ParsingMode, ToCss};

use crate::{
    context::QuirksMode,
    custom_properties::AttrTaint,
    parser::ParserContext,
    properties::{
        parse_one_declaration_into, parse_property_declaration_list, AllShorthand, Importance,
        PropertyDeclaration, PropertyDeclarationBlock, PropertyId, SourcePropertyDeclaration,
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
            .filter_map(|(declaration, importance)| declaration_entry(declaration, importance))
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

    pub fn set_property(
        &mut self,
        name: &str,
        value: &str,
        priority: bool,
    ) -> Option<Vec<CssDeclarationEntry>> {
        let property = PropertyId::parse_enabled_for_all_content(name).ok()?;
        let importance = if priority {
            Importance::Important
        } else {
            Importance::Normal
        };
        let mut declarations = SourcePropertyDeclaration::default();
        with_declaration_context(|context| {
            parse_one_declaration_into(
                &mut declarations,
                property.clone(),
                value,
                Origin::Author,
                context.url_data,
                None,
                ParsingMode::DEFAULT,
                QuirksMode::NoQuirks,
                CssRuleType::Style,
            )
            .ok()
        })??;
        let entries = source_declaration_entries(&declarations, importance)?;
        if entries.is_empty() {
            return None;
        }

        self.remove_property_by_id(&property);
        self.block.extend(declarations.drain(), importance);
        Some(entries)
    }

    pub fn remove_property(&mut self, name: &str) -> Option<bool> {
        let property = PropertyId::parse_enabled_for_all_content(name).ok()?;
        Some(self.remove_property_by_id(&property))
    }

    pub fn into_inner(self) -> PropertyDeclarationBlock {
        self.block
    }

    fn remove_property_by_id(&mut self, property: &PropertyId) -> bool {
        let Some(first_declaration) = self.block.first_declaration_to_remove(property) else {
            return false;
        };
        self.block.remove_property(property, first_declaration);
        true
    }
}

fn declaration_entry(
    declaration: &PropertyDeclaration,
    importance: Importance,
) -> Option<CssDeclarationEntry> {
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
}

fn source_declaration_entries(
    declarations: &SourcePropertyDeclaration,
    importance: Importance,
) -> Option<Vec<CssDeclarationEntry>> {
    if !matches!(declarations.all_shorthand, AllShorthand::NotSet) {
        let mut block = PropertyDeclarationBlock::new();
        for declaration in declarations.all_shorthand.declarations() {
            block.push(declaration, importance);
        }
        let mut value = CssString::new();
        block
            .property_value_to_css(
                &PropertyId::parse_enabled_for_all_content("all").ok()?,
                &mut value,
            )
            .ok()?;
        return (!value.is_empty()).then(|| {
            vec![CssDeclarationEntry {
                name: "all".to_owned(),
                value,
                priority: importance.important(),
            }]
        });
    }
    declarations
        .declarations
        .iter()
        .map(|declaration| declaration_entry(declaration, importance))
        .collect()
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

    #[test]
    fn declaration_block_exposes_lightmount_cssom_compat_properties() {
        static_prefs::set_pref!("layout.columns.enabled", true);
        let block = parse_declaration_block(
            "column-rule-width: 0; column-width: 0; scroll-margin-top: 0; \
             scroll-padding-bottom: 0; scroll-snap-align: start start; \
             scrollbar-color: auto; scrollbar-width: thin; shape-margin: 0; \
             appearance: auto; user-select: none; print-color-adjust: economy; \
             color-adjust: exact; forced-color-adjust: preserve-parent-color; \
             color-scheme: dark only; orphans: 2; widows: 3; \
             page-break-after: always; page-break-before: avoid; \
             page-break-inside: avoid; alignment-baseline: alphabetic; \
             background-attachment: local; baseline-source: first; \
             bookmark-level: 1; bookmark-state: closed; border-collapse: collapse; \
             caption-side: bottom; clear: both; clip: rect(0px, 1px, 2px, 3px); \
             empty-cells: hide; link-parameters: param(--a, orange), param(--b); \
             list-style-position: inside; list-style-type: upper-alpha; \
             outline-style: auto; table-layout: fixed; \
             text-size-adjust: calc(10% * sibling-index()); text-transform: uppercase;",
        );

        assert_eq!(
            block.property_value("column-rule-width").as_deref(),
            Some("0px")
        );
        assert_eq!(block.property_value("column-width").as_deref(), Some("0px"));
        assert_eq!(
            block.property_value("scroll-margin-top").as_deref(),
            Some("0px")
        );
        assert_eq!(
            block.property_value("scroll-padding-bottom").as_deref(),
            Some("0px")
        );
        assert_eq!(
            block.property_value("scroll-snap-align").as_deref(),
            Some("start")
        );
        assert_eq!(
            block.property_value("scrollbar-color").as_deref(),
            Some("auto")
        );
        assert_eq!(
            block.property_value("scrollbar-width").as_deref(),
            Some("thin")
        );
        assert_eq!(block.property_value("shape-margin").as_deref(), Some("0px"));
        assert_eq!(block.property_value("appearance").as_deref(), Some("auto"));
        assert_eq!(block.property_value("user-select").as_deref(), Some("none"));
        assert_eq!(
            block.property_value("color-adjust").as_deref(),
            Some("exact")
        );
        assert_eq!(
            block.property_value("print-color-adjust").as_deref(),
            Some("exact")
        );
        assert_eq!(
            block.property_value("forced-color-adjust").as_deref(),
            Some("preserve-parent-color")
        );
        assert_eq!(
            block.property_value("color-scheme").as_deref(),
            Some("dark only")
        );
        assert_eq!(block.property_value("orphans").as_deref(), Some("2"));
        assert_eq!(block.property_value("widows").as_deref(), Some("3"));
        assert_eq!(
            block.property_value("page-break-after").as_deref(),
            Some("always")
        );
        assert_eq!(
            block.property_value("page-break-before").as_deref(),
            Some("avoid")
        );
        assert_eq!(
            block.property_value("page-break-inside").as_deref(),
            Some("avoid")
        );
        assert_eq!(
            block.property_value("alignment-baseline").as_deref(),
            Some("alphabetic")
        );
        assert_eq!(
            block.property_value("background-attachment").as_deref(),
            Some("local")
        );
        assert_eq!(
            block.property_value("baseline-source").as_deref(),
            Some("first")
        );
        assert_eq!(block.property_value("bookmark-level").as_deref(), Some("1"));
        assert_eq!(
            block.property_value("bookmark-state").as_deref(),
            Some("closed")
        );
        assert_eq!(
            block.property_value("border-collapse").as_deref(),
            Some("collapse")
        );
        assert_eq!(
            block.property_value("caption-side").as_deref(),
            Some("bottom")
        );
        assert_eq!(block.property_value("clear").as_deref(), Some("both"));
        assert_eq!(
            block.property_value("clip").as_deref(),
            Some("rect(0px, 1px, 2px, 3px)")
        );
        assert_eq!(block.property_value("empty-cells").as_deref(), Some("hide"));
        assert_eq!(
            block.property_value("link-parameters").as_deref(),
            Some("param(--a, orange), param(--b)")
        );
        assert_eq!(
            block.property_value("list-style-position").as_deref(),
            Some("inside")
        );
        assert_eq!(
            block.property_value("list-style-type").as_deref(),
            Some("upper-alpha")
        );
        assert_eq!(
            block.property_value("outline-style").as_deref(),
            Some("auto")
        );
        assert_eq!(
            block.property_value("table-layout").as_deref(),
            Some("fixed")
        );
        assert_eq!(
            block.property_value("text-size-adjust").as_deref(),
            Some("calc(10% * sibling-index())")
        );
        assert_eq!(
            block.property_value("text-transform").as_deref(),
            Some("uppercase")
        );
    }

    #[test]
    fn declaration_block_handles_lightmount_cssom_compat_edge_values() {
        let block = parse_declaration_block(
            "text-size-adjust: calc(10% + 5%); \
             link-parameters: param(--a, ); \
             bookmark-level: none;",
        );

        assert_eq!(
            block.property_value("text-size-adjust").as_deref(),
            Some("calc(15%)")
        );
        assert_eq!(
            block.property_value("link-parameters").as_deref(),
            Some("param(--a, )")
        );
        assert_eq!(
            block.property_value("bookmark-level").as_deref(),
            Some("none")
        );

        let block = parse_declaration_block(
            "text-size-adjust: calc(10% * sibling-index()); \
             link-parameters: param(--a);",
        );
        assert_eq!(
            block.property_value("text-size-adjust").as_deref(),
            Some("calc(10% * sibling-index())")
        );
        assert_eq!(
            block.property_value("link-parameters").as_deref(),
            Some("param(--a)")
        );

        let block = parse_declaration_block("link-parameters: param(--a");
        assert_eq!(
            block.property_value("link-parameters").as_deref(),
            Some("param(--a)")
        );
    }

    #[test]
    fn declaration_block_rejects_invalid_lightmount_cssom_compat_values() {
        let block = parse_declaration_block(
            "bookmark-level: 0; bookmark-state: none; \
             text-size-adjust: -100%; text-size-adjust: 10px; \
             link-parameters: param(-a); link-parameters: param(--a red); \
             link-parameters: param(--a, red) param(--b, blue);",
        );

        assert_eq!(block.property_value("bookmark-level").as_deref(), Some(""));
        assert_eq!(block.property_value("bookmark-state").as_deref(), Some(""));
        assert_eq!(
            block.property_value("text-size-adjust").as_deref(),
            Some("")
        );
        assert_eq!(block.property_value("link-parameters").as_deref(), Some(""));
    }

    #[test]
    fn declaration_block_set_property_updates_through_pdb() {
        let mut block = parse_declaration_block("color: red !important; padding: 1px 2px;");

        let entries = block
            .set_property("color", "blue", false)
            .expect("color should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "color");
        assert_eq!(entries[0].value, "blue");
        assert_eq!(entries[0].priority, false);
        assert_eq!(block.property_value("color").as_deref(), Some("blue"));
        assert!(!block.property_priority("color"));

        let entries = block
            .set_property("margin", "0 2px", true)
            .expect("margin shorthand should parse");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["margin-top", "margin-right", "margin-bottom", "margin-left"]
        );
        assert_eq!(block.property_value("margin").as_deref(), Some("0px 2px"));
        assert!(block.property_priority("margin"));
        assert_eq!(
            block.css_text(),
            "padding: 1px 2px; color: blue; margin: 0px 2px !important;"
        );

        let entries = block
            .set_property("link-parameters", "param(--a", false)
            .expect("EOF-recovered CSSOM values should update through PDB");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "link-parameters");
        assert_eq!(entries[0].value, "param(--a)");
        assert_eq!(entries[0].priority, false);
        assert_eq!(
            block.property_value("link-parameters").as_deref(),
            Some("param(--a)")
        );
        assert!(!block.property_priority("link-parameters"));

        let mut longhand_block = parse_declaration_block("");
        for name in ["margin-top", "margin-right", "margin-bottom", "margin-left"] {
            longhand_block
                .set_property(name, "0", true)
                .expect("margin longhand should parse");
        }
        assert_eq!(
            longhand_block.property_value("margin").as_deref(),
            Some("0px")
        );
        assert!(longhand_block.property_priority("margin"));
        assert_eq!(longhand_block.css_text(), "margin: 0px !important;");

        let mut single_longhand_block = parse_declaration_block("");
        single_longhand_block
            .set_property("opacity", "0.5", true)
            .expect("opacity should parse");
        assert_eq!(
            single_longhand_block.property_value("opacity").as_deref(),
            Some("0.5")
        );
        assert!(single_longhand_block.property_priority("opacity"));
        assert_eq!(single_longhand_block.css_text(), "opacity: 0.5 !important;");

        let mut all_block = parse_declaration_block("display: block; color: red;");
        let entries = all_block
            .set_property("all", "inherit", false)
            .expect("all shorthand should parse as a CSSOM value fragment");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "all");
        assert_eq!(entries[0].value, "inherit");
        assert_eq!(entries[0].priority, false);
        assert_eq!(all_block.css_text(), "all: inherit;");
    }

    #[test]
    fn declaration_block_set_property_rejects_declaration_source_fragments() {
        let mut block = parse_declaration_block("color: red;");

        assert!(block
            .set_property("color", "blue; width: 1px", false)
            .is_none());
        assert_eq!(block.css_text(), "color: red;");
    }

    #[test]
    fn declaration_block_remove_property_uses_cssom_shorthand_semantics() {
        let mut block = parse_declaration_block("padding: 1px 2px; color: red;");

        assert_eq!(block.remove_property("padding-left"), Some(true));
        assert_eq!(block.property_value("padding").as_deref(), Some(""));
        assert_eq!(block.property_value("padding-left").as_deref(), Some(""));
        assert_eq!(block.property_value("padding-top").as_deref(), Some("1px"));

        assert_eq!(block.remove_property("padding"), Some(true));
        assert_eq!(block.property_value("padding-top").as_deref(), Some(""));
        assert_eq!(block.property_value("padding-right").as_deref(), Some(""));
        assert_eq!(block.remove_property("does-not-exist"), None);
    }
}
