//! Lightmount-facing stylesheet rule tree hooks.

use crate::{
    context::QuirksMode,
    media_queries::MediaList,
    shared_lock::{SharedRwLock, ToCssWithGuard},
    stylesheets::{
        import_rule::{ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition},
        keyframes_rule::{Keyframe, KeyframeSelectors},
        AllowImportRules, CssRule, CssRuleType, CssRuleTypes, CssRules, Origin, RulesMutateError,
        Stylesheet, StylesheetContents, StylesheetLoader, UrlExtraData,
    },
    values::CssUrl,
};
use cssparser::{Parser, ParserInput, SourceLocation};
use servo_arc::Arc;
use std::sync::atomic::AtomicBool;
use style_traits::ToCss;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssStylesheetMutationResult {
    pub css_text: String,
    pub rules: Vec<CssStylesheetRuleView>,
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
    let parsed =
        parse_stylesheet_rule_for_insert_rule(existing_rule_texts, rule_text, index, constructed)?;
    let guard = parsed.shared_lock.read();
    Ok(CssStylesheetRuleText {
        rule_type: parsed.rule.rule_type(),
        css_text: parsed.rule.to_css_string(&guard),
    })
}

pub fn insert_stylesheet_rule(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed =
        parse_stylesheet_rule_for_insert_rule(existing_rule_texts, rule_text, index, constructed)?;
    {
        let mut guard = parsed.shared_lock.write();
        parsed
            .contents
            .rules
            .write_with(&mut guard)
            .0
            .insert(index, parsed.rule);
    }
    Ok(stylesheet_mutation_result(
        &parsed.contents,
        &parsed.shared_lock,
    ))
}

pub fn delete_stylesheet_rule(
    existing_rule_texts: &[String],
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_stylesheet_for_mutation(existing_rule_texts, constructed)?;
    {
        let mut guard = parsed.shared_lock.write();
        parsed
            .contents
            .rules
            .write_with(&mut guard)
            .remove_rule(index)
            .map_err(CssRuleInsertError::from)?;
    }
    Ok(stylesheet_mutation_result(
        &parsed.contents,
        &parsed.shared_lock,
    ))
}

pub fn insert_nested_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_nested_rules_for_mutation(
        parent_stylesheet_rule_texts,
        existing_rule_texts,
        containing_rule_type_bits,
        parse_relative_rule_type,
    )?;
    if index > parsed.rules.0.len() {
        return Err(CssRuleInsertError::IndexSize);
    }
    let rule = parsed.parse_rule_for_insert(rule_text, index)?;
    let mut rules = parsed.rules;
    rules.0.insert(index, rule);
    Ok(css_rules_mutation_result(&rules, &parsed.shared_lock))
}

pub fn delete_nested_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_nested_rules_for_mutation(
        parent_stylesheet_rule_texts,
        existing_rule_texts,
        containing_rule_type_bits,
        parse_relative_rule_type,
    )?;
    let mut rules = parsed.rules;
    rules.remove_rule(index).map_err(CssRuleInsertError::from)?;
    Ok(css_rules_mutation_result(&rules, &parsed.shared_lock))
}

pub fn insert_keyframe_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed =
        parse_keyframe_rules_for_mutation(parent_stylesheet_rule_texts, existing_rule_texts)?;
    if index > parsed.keyframes.len() {
        return Err(CssRuleInsertError::IndexSize);
    }
    let rule = Keyframe::parse(rule_text, &parsed.contents, &parsed.shared_lock)
        .map_err(|_| CssRuleInsertError::Syntax)?;
    let mut keyframes = parsed.keyframes;
    keyframes.insert(index, rule);
    Ok(keyframe_rules_mutation_result(
        &keyframes,
        &parsed.shared_lock,
    ))
}

pub fn delete_keyframe_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed =
        parse_keyframe_rules_for_mutation(parent_stylesheet_rule_texts, existing_rule_texts)?;
    if index >= parsed.keyframes.len() {
        return Err(CssRuleInsertError::IndexSize);
    }
    let mut keyframes = parsed.keyframes;
    keyframes.remove(index);
    Ok(keyframe_rules_mutation_result(
        &keyframes,
        &parsed.shared_lock,
    ))
}

pub fn normalize_keyframe_selector_text(selector_text: &str) -> Option<String> {
    parse_keyframe_selectors(selector_text).map(|selectors| selectors.to_css_string())
}

pub fn keyframe_selector_texts_match(existing_selector_text: &str, selector_text: &str) -> bool {
    let Some(existing) = parse_keyframe_selectors(existing_selector_text) else {
        return false;
    };
    let Some(selector) = parse_keyframe_selectors(selector_text) else {
        return false;
    };
    existing == selector
}

struct ParsedStylesheetForMutation {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    allow_import_rules: AllowImportRules,
}

struct ParsedRuleForInsert {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    rule: CssRule,
}

struct ParsedNestedRulesForMutation {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    rules: CssRules,
    containing_rule_types: CssRuleTypes,
    parse_relative_rule_type: Option<CssRuleType>,
}

struct ParsedKeyframeRulesForMutation {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    keyframes: Vec<Arc<crate::shared_lock::Locked<Keyframe>>>,
}

impl ParsedNestedRulesForMutation {
    fn parse_rule_for_insert(
        &self,
        rule_text: &str,
        index: usize,
    ) -> Result<CssRule, CssRuleInsertError> {
        self.rules
            .parse_rule_for_insert(
                &self.shared_lock,
                rule_text,
                &self.contents,
                index,
                self.containing_rule_types,
                self.parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)
    }
}

fn parse_stylesheet_rule_for_insert_rule(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<ParsedRuleForInsert, CssRuleInsertError> {
    let parsed = parse_stylesheet_for_mutation(existing_rule_texts, constructed)?;
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match parsed.allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let guard = parsed.shared_lock.read();
    let rules = parsed.contents.rules.read_with(&guard);
    let parsed_rule = rules.parse_rule_for_insert(
        &parsed.shared_lock,
        rule_text,
        &parsed.contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        parsed.allow_import_rules,
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
    drop(guard);
    Ok(ParsedRuleForInsert {
        contents: parsed.contents,
        shared_lock: parsed.shared_lock,
        rule,
    })
}

fn parse_stylesheet_for_mutation(
    existing_rule_texts: &[String],
    constructed: bool,
) -> Result<ParsedStylesheetForMutation, CssRuleInsertError> {
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
    Ok(ParsedStylesheetForMutation {
        contents,
        shared_lock,
        allow_import_rules,
    })
}

fn parse_nested_rules_for_mutation(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<ParsedNestedRulesForMutation, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let parent_css = parent_stylesheet_rule_texts.join(" ");
    let contents = StylesheetContents::from_str(
        &parent_css,
        url_data,
        Origin::Author,
        &shared_lock,
        Some(&import_loader as &dyn StylesheetLoader),
        None,
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
        None,
    );
    let containing_rule_types = CssRuleTypes::from_bits(containing_rule_type_bits);
    let mut rules = CssRules(Vec::new());
    for rule_text in existing_rule_texts {
        let rule = rules
            .parse_rule_for_insert(
                &shared_lock,
                rule_text,
                &contents,
                rules.0.len(),
                containing_rule_types,
                parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)?;
        rules.0.push(rule);
    }
    Ok(ParsedNestedRulesForMutation {
        contents,
        shared_lock,
        rules,
        containing_rule_types,
        parse_relative_rule_type,
    })
}

fn parse_keyframe_rules_for_mutation(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
) -> Result<ParsedKeyframeRulesForMutation, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let parent_css = parent_stylesheet_rule_texts.join(" ");
    let contents = StylesheetContents::from_str(
        &parent_css,
        url_data,
        Origin::Author,
        &shared_lock,
        Some(&import_loader as &dyn StylesheetLoader),
        None,
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
        None,
    );
    let mut keyframes = Vec::new();
    for rule_text in existing_rule_texts {
        let rule = Keyframe::parse(rule_text, &contents, &shared_lock)
            .map_err(|_| CssRuleInsertError::Syntax)?;
        keyframes.push(rule);
    }
    Ok(ParsedKeyframeRulesForMutation {
        contents,
        shared_lock,
        keyframes,
    })
}

fn parse_keyframe_selectors(selector_text: &str) -> Option<KeyframeSelectors> {
    let mut input = ParserInput::new(selector_text);
    let mut input = Parser::new(&mut input);
    input.parse_entirely(KeyframeSelectors::parse).ok()
}

fn stylesheet_mutation_result(
    contents: &StylesheetContents,
    shared_lock: &SharedRwLock,
) -> CssStylesheetMutationResult {
    let guard = shared_lock.read();
    let rules = contents.rules.read_with(&guard);
    css_rules_mutation_result(rules, shared_lock)
}

fn css_rules_mutation_result(
    rules: &CssRules,
    shared_lock: &SharedRwLock,
) -> CssStylesheetMutationResult {
    let guard = shared_lock.read();
    let rules = rules
        .0
        .iter()
        .map(|rule| stylesheet_rule_view(rule, &guard))
        .collect::<Vec<_>>();
    let css_text = rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    CssStylesheetMutationResult { css_text, rules }
}

fn keyframe_rules_mutation_result(
    keyframes: &[Arc<crate::shared_lock::Locked<Keyframe>>],
    shared_lock: &SharedRwLock,
) -> CssStylesheetMutationResult {
    let guard = shared_lock.read();
    let rules = keyframes
        .iter()
        .map(|rule| keyframe_rule_view(rule, &guard))
        .collect::<Vec<_>>();
    let css_text = rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    CssStylesheetMutationResult { css_text, rules }
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
        child_rules: stylesheet_rule_child_views(rule, guard),
    }
}

fn stylesheet_rule_child_views(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Vec<CssStylesheetRuleView> {
    match rule {
        CssRule::Keyframes(rule) => rule
            .read_with(guard)
            .keyframes
            .iter()
            .map(|rule| keyframe_rule_view(rule, guard))
            .collect(),
        _ => rule
            .children(guard)
            .iter()
            .map(|rule| stylesheet_rule_view(rule, guard))
            .collect(),
    }
}

fn keyframe_rule_view(
    rule: &Arc<crate::shared_lock::Locked<Keyframe>>,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssStylesheetRuleView {
    let rule = rule.read_with(guard);
    CssStylesheetRuleView {
        rule_type: CssRuleType::Keyframe,
        css_text: rule.to_css_string(guard),
        child_rules: Vec::new(),
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
        delete_keyframe_rule, delete_nested_rule, delete_stylesheet_rule, insert_keyframe_rule,
        insert_nested_rule, insert_stylesheet_rule, keyframe_selector_texts_match,
        normalize_keyframe_selector_text, parse_constructed_stylesheet_rule_texts,
        parse_stylesheet_rule_for_insert, parse_stylesheet_rule_texts, parse_stylesheet_rule_views,
        serialize_stylesheet, CssRuleInsertError,
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
    fn stylesheet_rule_views_include_keyframes_children() {
        let rules = parse_stylesheet_rule_views(
            "@keyframes slide { from { opacity: 0; } to { opacity: 1; } }",
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::Keyframes);
        assert_eq!(rules[0].child_rules.len(), 2);
        assert_eq!(rules[0].child_rules[0].rule_type, CssRuleType::Keyframe);
        assert_eq!(rules[0].child_rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(rules[0].child_rules[1].css_text, "100% { opacity: 1; }");
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

    #[test]
    fn insert_stylesheet_rule_returns_mutated_rule_tree_view() {
        let existing = vec![
            String::from("@import url(\"a.css\");"),
            String::from(".one { color: red; }"),
        ];

        let mutation = insert_stylesheet_rule(
            &existing,
            "@media screen { .two { padding: 0 1px; } }",
            2,
            false,
        )
        .expect("media rule should insert");

        assert_eq!(mutation.rules.len(), 3);
        assert_eq!(
            mutation.css_text,
            "@import url(\"a.css\"); .one { color: red; } @media screen {\n  .two { padding: 0px 1px; }\n}"
        );
        assert_eq!(mutation.rules[2].rule_type, CssRuleType::Media);
        assert_eq!(mutation.rules[2].child_rules.len(), 1);
        assert_eq!(
            mutation.rules[2].child_rules[0].css_text,
            ".two { padding: 0px 1px; }"
        );
    }

    #[test]
    fn delete_stylesheet_rule_uses_stylo_remove_semantics() {
        let existing = vec![
            String::from("@namespace svg url(\"http://www.w3.org/2000/svg\");"),
            String::from(".one { color: red; }"),
            String::from(".two { color: blue; }"),
        ];

        assert_eq!(
            delete_stylesheet_rule(&existing, 0, false),
            Err(CssRuleInsertError::InvalidState)
        );

        let mutation =
            delete_stylesheet_rule(&existing, 1, false).expect("style rule should be removable");
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(
            mutation.css_text,
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .two { color: blue; }"
        );
    }

    #[test]
    fn nested_rule_mutation_returns_media_rule_tree_views() {
        let existing = vec![String::from(".one { color: red; }")];
        let inserted = insert_nested_rule(
            &[],
            &existing,
            "@supports (display: grid) { .two { display: grid; } }",
            1,
            CssRuleType::Media.bit(),
            None,
        )
        .expect("supports rule should insert into media rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(
            inserted.css_text,
            ".one { color: red; } @supports (display: grid) {\n  .two { display: grid; }\n}"
        );
        assert_eq!(inserted.rules[1].rule_type, CssRuleType::Supports);
        assert_eq!(inserted.rules[1].child_rules.len(), 1);
        assert_eq!(
            inserted.rules[1].child_rules[0].css_text,
            ".two { display: grid; }"
        );

        let deleted = delete_nested_rule(
            &[],
            &inserted
                .rules
                .iter()
                .map(|rule| rule.css_text.clone())
                .collect::<Vec<_>>(),
            0,
            CssRuleType::Media.bit(),
            None,
        )
        .expect("nested style rule should delete");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(
            deleted.css_text,
            "@supports (display: grid) {\n  .two { display: grid; }\n}"
        );
    }

    #[test]
    fn nested_style_rule_mutation_parses_relative_selectors_and_declarations() {
        let parent_context = vec![String::from(
            "@namespace svg url(\"http://www.w3.org/2000/svg\");",
        )];
        let existing = vec![String::from("& .one { color: red; }")];

        let inserted = insert_nested_rule(
            &parent_context,
            &existing,
            "> svg|path { color: blue; }",
            1,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("relative style rule should insert into style rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[0].css_text, "& .one { color: red; }");
        assert_eq!(inserted.rules[1].css_text, "& > svg|path { color: blue; }");

        let declarations = insert_nested_rule(
            &parent_context,
            &inserted
                .rules
                .iter()
                .map(|rule| rule.css_text.clone())
                .collect::<Vec<_>>(),
            "margin: 0; padding: 1px;",
            0,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("declarations should insert as nested declarations");

        assert_eq!(declarations.rules.len(), 3);
        assert_eq!(
            declarations.rules[0].rule_type,
            CssRuleType::NestedDeclarations
        );
        assert_eq!(declarations.rules[0].css_text, "margin: 0px; padding: 1px;");
    }

    #[test]
    fn keyframe_rule_mutation_returns_keyframe_views() {
        let existing = vec![String::from("0% { opacity: 0; }")];
        let inserted = insert_keyframe_rule(
            &[],
            &existing,
            "to { opacity: 1; transform: translateX(10px); }",
            1,
        )
        .expect("keyframe rule should insert into keyframes rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(
            inserted.css_text,
            "0% { opacity: 0; } 100% { opacity: 1; transform: translateX(10px); }"
        );
        assert_eq!(inserted.rules[1].rule_type, CssRuleType::Keyframe);
        assert_eq!(
            inserted.rules[1].css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );

        let deleted = delete_keyframe_rule(
            &[],
            &inserted
                .rules
                .iter()
                .map(|rule| rule.css_text.clone())
                .collect::<Vec<_>>(),
            0,
        )
        .expect("keyframe rule should delete");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(
            deleted.css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );
    }

    #[test]
    fn keyframe_rule_mutation_rejects_invalid_keyframe_rules() {
        assert_eq!(
            insert_keyframe_rule(&[], &[], ".not-a-keyframe { opacity: 1; }", 0),
            Err(CssRuleInsertError::Syntax)
        );
        assert_eq!(
            delete_keyframe_rule(&[], &[], 0),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn keyframe_selector_helpers_use_stylo_parser_and_serialization() {
        assert_eq!(normalize_keyframe_selector_text("from"), Some("0%".into()));
        assert_eq!(
            normalize_keyframe_selector_text("50%, to"),
            Some("50%, 100%".into())
        );
        assert_eq!(normalize_keyframe_selector_text("body"), None);
        assert_eq!(normalize_keyframe_selector_text("-1%"), None);

        assert!(keyframe_selector_texts_match("from", "0%"));
        assert!(keyframe_selector_texts_match("50%, to", "50%, 100%"));
        assert!(!keyframe_selector_texts_match("50%, to", "50%"));
        assert!(!keyframe_selector_texts_match("body", "body"));
    }
}
