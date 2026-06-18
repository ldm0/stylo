//! Lightmount-facing stylesheet rule tree hooks.

use crate::{
    context::QuirksMode,
    media_queries::MediaList,
    shared_lock::{SharedRwLock, ToCssWithGuard},
    stylesheets::{
        import_rule::{ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition},
        AllowImportRules, CssRule, CssRuleType, CssRuleTypes, Origin, RulesMutateError, Stylesheet,
        StylesheetContents, StylesheetLoader, UrlExtraData,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssStylesheetRuleView {
    pub rule_type: CssRuleType,
    pub css_text: String,
    pub child_rules: Vec<CssStylesheetRuleView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssRuleInsertError {
    Syntax,
    IndexSize,
    HierarchyRequest,
    InvalidState,
}

pub fn parse_stylesheet_rule_texts(css_text: &str) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_texts_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_texts(css_text: &str) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_texts_with_import_policy(css_text, AllowImportRules::No)
}

pub fn parse_stylesheet_rule_views(css_text: &str) -> Vec<CssStylesheetRuleView> {
    parse_stylesheet_rule_views_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_views(css_text: &str) -> Vec<CssStylesheetRuleView> {
    parse_stylesheet_rule_views_with_import_policy(css_text, AllowImportRules::No)
}

pub fn serialize_stylesheet(css_text: &str) -> String {
    parse_stylesheet_rule_texts(css_text)
        .into_iter()
        .map(|rule| rule.css_text)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_stylesheet_rule_for_insert(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetRuleText, CssRuleInsertError> {
    let allow_import_rules = if constructed {
        AllowImportRules::No
    } else {
        AllowImportRules::Yes
    };
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let existing_css = existing_rule_texts.join(" ");
    let contents = StylesheetContents::from_str(
        &existing_css,
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
    let rules = contents.rules.read_with(&guard);
    let parsed_rule = rules.parse_rule_for_insert(
        &shared_lock,
        rule_text,
        &contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        allow_import_rules,
    );
    let rule = match parsed_rule {
        Ok(rule) => rule,
        Err(error) => {
            let error = CssRuleInsertError::from(error);
            if error == CssRuleInsertError::HierarchyRequest
                && rule_text_is_namespace_rule(rule_text)
                && rules
                    .0
                    .iter()
                    .any(|rule| !matches!(rule, CssRule::Import(..) | CssRule::Namespace(..)))
            {
                return Err(CssRuleInsertError::InvalidState);
            }
            return Err(error);
        },
    };
    Ok(CssStylesheetRuleText {
        rule_type: rule.rule_type(),
        css_text: rule.to_css_string(&guard),
    })
}

fn parse_stylesheet_rule_texts_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_views_with_import_policy(css_text, allow_import_rules)
        .into_iter()
        .map(|rule| CssStylesheetRuleText {
            rule_type: rule.rule_type,
            css_text: rule.css_text,
        })
        .collect()
}

fn parse_stylesheet_rule_views_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> Vec<CssStylesheetRuleView> {
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
        .map(|rule| stylesheet_rule_view(rule, &guard))
        .collect()
}

fn stylesheet_rule_view(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssStylesheetRuleView {
    CssStylesheetRuleView {
        rule_type: rule.rule_type(),
        css_text: rule.to_css_string(guard),
        child_rules: rule
            .children(guard)
            .iter()
            .map(|rule| stylesheet_rule_view(rule, guard))
            .collect(),
    }
}

fn about_blank_url_data() -> Option<UrlExtraData> {
    Some(UrlExtraData::from(url::Url::parse("about:blank").ok()?))
}

fn rule_text_is_namespace_rule(rule_text: &str) -> bool {
    parse_stylesheet_rule_texts_with_import_policy(rule_text, AllowImportRules::Yes)
        .as_slice()
        .is_single_rule_type(CssRuleType::Namespace)
}

trait SingleRuleType {
    fn is_single_rule_type(&self, rule_type: CssRuleType) -> bool;
}

impl SingleRuleType for [CssStylesheetRuleText] {
    fn is_single_rule_type(&self, rule_type: CssRuleType) -> bool {
        matches!(self, [rule] if rule.rule_type == rule_type)
    }
}

impl From<RulesMutateError> for CssRuleInsertError {
    fn from(value: RulesMutateError) -> Self {
        match value {
            RulesMutateError::Syntax => Self::Syntax,
            RulesMutateError::IndexSize => Self::IndexSize,
            RulesMutateError::HierarchyRequest => Self::HierarchyRequest,
            RulesMutateError::InvalidState => Self::InvalidState,
        }
    }
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
        parse_constructed_stylesheet_rule_texts, parse_stylesheet_rule_for_insert,
        parse_stylesheet_rule_texts, parse_stylesheet_rule_views, serialize_stylesheet,
        CssRuleInsertError,
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
    fn stylesheet_rule_views_include_stylo_nested_children() {
        let rules = parse_stylesheet_rule_views(
            ".one { color: red; } @media screen { .two { margin: 0; } @supports (display: grid) { .three { display: grid; } } }",
        );

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_type, CssRuleType::Style);
        assert!(rules[0].child_rules.is_empty());
        assert_eq!(rules[1].rule_type, CssRuleType::Media);
        assert_eq!(
            rules[1].css_text,
            "@media screen {\n  .two { margin: 0px; }\n  @supports (display: grid) {\n  .three { display: grid; }\n}\n}"
        );
        assert_eq!(rules[1].child_rules.len(), 2);
        assert_eq!(rules[1].child_rules[0].rule_type, CssRuleType::Style);
        assert_eq!(rules[1].child_rules[0].css_text, ".two { margin: 0px; }");
        assert_eq!(rules[1].child_rules[1].rule_type, CssRuleType::Supports);
        assert_eq!(rules[1].child_rules[1].child_rules.len(), 1);
        assert_eq!(
            rules[1].child_rules[1].child_rules[0].css_text,
            ".three { display: grid; }"
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

    #[test]
    fn insert_rule_parser_uses_stylo_ordering_and_serialization() {
        let existing = vec![
            String::from("@import url(\"a.css\");"),
            String::from("@namespace svg url(\"http://www.w3.org/2000/svg\");"),
            String::from(".one { color: red; }"),
        ];

        let inserted = parse_stylesheet_rule_for_insert(&existing, ".two { margin: 0; }", 3, false)
            .expect("style rule should insert");
        assert_eq!(inserted.rule_type, CssRuleType::Style);
        assert_eq!(inserted.css_text, ".two { margin: 0px; }");

        assert_eq!(
            parse_stylesheet_rule_for_insert(&existing, "@import url(\"late.css\");", 3, false),
            Err(CssRuleInsertError::HierarchyRequest)
        );
        assert_eq!(
            parse_stylesheet_rule_for_insert(
                &existing,
                "@namespace html url(\"http://www.w3.org/1999/xhtml\");",
                3,
                false,
            ),
            Err(CssRuleInsertError::InvalidState)
        );
        assert_eq!(
            parse_stylesheet_rule_for_insert(&existing, ".too-far {}", 4, false),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn constructed_insert_rule_parser_rejects_import_rules() {
        assert_eq!(
            parse_stylesheet_rule_for_insert(&[], "@import url(\"ignored.css\");", 0, true),
            Err(CssRuleInsertError::Syntax)
        );
    }
}
