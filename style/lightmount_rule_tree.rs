//! Lightmount-facing stylesheet rule tree hooks.

use crate::{
    context::QuirksMode,
    media_queries::MediaList,
    shared_lock::{SharedRwLock, ToCssWithGuard},
    stylesheets::{
        import_rule::{ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition},
        AllowImportRules, CssRuleType, Origin, Stylesheet, StylesheetContents, StylesheetLoader,
        UrlExtraData,
    },
    values::CssUrl,
};
use cssparser::SourceLocation;
use servo_arc::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssStylesheetRuleText {
    pub rule_type: CssRuleType,
    pub css_text: String,
}

pub fn parse_stylesheet_rule_texts(css_text: &str) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_texts_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_texts(css_text: &str) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_texts_with_import_policy(css_text, AllowImportRules::No)
}

pub fn serialize_stylesheet(css_text: &str) -> String {
    parse_stylesheet_rule_texts(css_text)
        .into_iter()
        .map(|rule| rule.css_text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_stylesheet_rule_texts_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> Vec<CssStylesheetRuleText> {
    let Some(url_data) = about_blank_url_data() else {
        return Vec::new();
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let contents = StylesheetContents::from_str(
        css_text,
        url_data,
        Origin::Author,
        &shared_lock,
        stylesheet_loader,
        None,
        QuirksMode::NoQuirks,
        allow_import_rules,
        None,
    );
    let guard = shared_lock.read();
    contents
        .rules(&guard)
        .iter()
        .map(|rule| CssStylesheetRuleText {
            rule_type: rule.rule_type(),
            css_text: rule.to_css_string(&guard),
        })
        .collect()
}

fn about_blank_url_data() -> Option<UrlExtraData> {
    Some(UrlExtraData::from(url::Url::parse("about:blank").ok()?))
}

struct LightmountImportLoader;

impl StylesheetLoader for LightmountImportLoader {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: Arc<crate::shared_lock::Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> Arc<crate::shared_lock::Locked<ImportRule>> {
        let contents = StylesheetContents::from_str(
            "",
            about_blank_url_data().expect("static about:blank URL should parse"),
            Origin::Author,
            lock,
            None,
            None,
            QuirksMode::NoQuirks,
            AllowImportRules::No,
            None,
        );
        let stylesheet = Arc::new(Stylesheet {
            contents: lock.wrap(contents),
            shared_lock: lock.clone(),
            media,
            disabled: AtomicBool::new(false),
        });
        Arc::new(lock.wrap(ImportRule {
            url,
            stylesheet: ImportSheet::new(stylesheet),
            supports,
            layer,
            source_location: location,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_constructed_stylesheet_rule_texts, parse_stylesheet_rule_texts, serialize_stylesheet,
    };
    use crate::stylesheets::CssRuleType;

    #[test]
    fn stylesheet_rule_texts_use_stylo_rule_parser_and_serializer() {
        let rules = parse_stylesheet_rule_texts(
            "  @import url(\"a.css\"); .one { color: red; } @media screen { .two { margin: 0; } }",
        );

        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].rule_type, CssRuleType::Import);
        assert_eq!(rules[0].css_text, "@import url(\"a.css\");");
        assert_eq!(rules[1].rule_type, CssRuleType::Style);
        assert_eq!(rules[1].css_text, ".one { color: red; }");
        assert_eq!(rules[2].rule_type, CssRuleType::Media);
        assert_eq!(
            rules[2].css_text,
            "@media screen {\n  .two { margin: 0px; }\n}"
        );
    }

    #[test]
    fn constructed_stylesheet_rule_texts_drop_import_rules() {
        let rules = parse_constructed_stylesheet_rule_texts(
            "@import url(\"ignored.css\"); .target { color: blue; }",
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::Style);
        assert_eq!(rules[0].css_text, ".target { color: blue; }");
    }

    #[test]
    fn stylesheet_serialization_joins_stylo_serialized_rule_texts() {
        assert_eq!(
            serialize_stylesheet(".one { padding: 0 1px; } .two { display: block; }"),
            ".one { padding: 0px 1px; } .two { display: block; }"
        );
    }
}
