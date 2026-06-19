//! Lightmount-facing stylesheet rule tree hooks.

use std::{
    borrow::Cow,
    sync::{atomic::AtomicBool, Once},
};

use cssparser::{Parser, ParserInput, SourceLocation};
use servo_arc::Arc;
use style_traits::{ParsingMode, ToCss};

use crate::{
    context::QuirksMode,
    custom_properties::AttrTaint,
    media_queries::MediaList,
    parser::ParserContext,
    properties::{parse_property_declaration_list, PropertyDeclarationBlock},
    shared_lock::{SharedRwLock, ToCssWithGuard},
    stylesheets::{
        import_rule::{ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition},
        keyframes_rule::{Keyframe, KeyframeSelectors, KeyframesRule},
        AllowImportRules, CssRule, CssRuleType, CssRuleTypes, CssRules, Namespaces, Origin,
        RulesMutateError, Stylesheet, StylesheetContents, StylesheetLoader, UrlExtraData,
    },
    values::CssUrl,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssNestedRuleMutationResult {
    pub stylesheet_css_text: String,
    pub parent_rule: CssStylesheetRuleView,
    pub rules: Vec<CssStylesheetRuleView>,
}

#[derive(Clone)]
pub struct CssStylesheetRuleTree {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    allow_import_rules: AllowImportRules,
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

pub fn parse_stylesheet_rule_tree(css_text: &str) -> CssStylesheetRuleTree {
    parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_tree(css_text: &str) -> CssStylesheetRuleTree {
    parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No)
}

pub fn stylesheet_rule_tree_css_text(rule_tree: &CssStylesheetRuleTree) -> String {
    stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock).css_text
}

pub fn stylesheet_rule_tree_rule_views(
    rule_tree: &CssStylesheetRuleTree,
) -> Vec<CssStylesheetRuleView> {
    stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock).rules
}

pub fn insert_rule_into_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_text: &str,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match rule_tree.allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let parsed_rule = rules.parse_rule_for_insert(
        &rule_tree.shared_lock,
        rule_text,
        &rule_tree.contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        rule_tree.allow_import_rules,
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
    let refresh_namespaces = matches!(&rule, CssRule::Namespace(..));
    {
        let mut guard = rule_tree.shared_lock.write();
        rule_tree
            .contents
            .rules
            .write_with(&mut guard)
            .0
            .insert(index, rule);
    }
    let result = stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock);
    if refresh_namespaces {
        refresh_stylesheet_rule_tree_from_css_text(rule_tree, &result.css_text);
        return Ok(stylesheet_mutation_result(
            &rule_tree.contents,
            &rule_tree.shared_lock,
        ));
    }
    Ok(result)
}

pub fn delete_rule_from_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let refresh_namespaces = {
        let guard = rule_tree.shared_lock.read();
        rule_tree
            .contents
            .rules
            .read_with(&guard)
            .0
            .get(index)
            .is_some_and(|rule| matches!(rule, CssRule::Namespace(..)))
    };
    {
        let mut guard = rule_tree.shared_lock.write();
        rule_tree
            .contents
            .rules
            .write_with(&mut guard)
            .remove_rule(index)
            .map_err(CssRuleInsertError::from)?;
    }
    let result = stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock);
    if refresh_namespaces {
        refresh_stylesheet_rule_tree_from_css_text(rule_tree, &result.css_text);
        return Ok(stylesheet_mutation_result(
            &rule_tree.contents,
            &rule_tree.shared_lock,
        ));
    }
    Ok(result)
}

pub fn replace_rule_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_text: &str,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match rule_tree.allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let Some(existing_rule) = rules.0.get(index) else {
        return Err(CssRuleInsertError::IndexSize);
    };
    let old_is_namespace = matches!(existing_rule, CssRule::Namespace(..));
    let parsed_rule = rules.parse_rule_for_insert(
        &rule_tree.shared_lock,
        rule_text,
        &rule_tree.contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        rule_tree.allow_import_rules,
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
    let refresh_namespaces = old_is_namespace || matches!(&rule, CssRule::Namespace(..));
    {
        let mut guard = rule_tree.shared_lock.write();
        rule_tree.contents.rules.write_with(&mut guard).0[index] = rule;
    }
    let result = stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock);
    if refresh_namespaces {
        refresh_stylesheet_rule_tree_from_css_text(rule_tree, &result.css_text);
        return Ok(stylesheet_mutation_result(
            &rule_tree.contents,
            &rule_tree.shared_lock,
        ));
    }
    Ok(result)
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

pub fn insert_nested_rule_into_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let child_rules = mutable_child_rules_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let containing_rule_types = CssRuleTypes::from_bits(containing_rule_type_bits);
    let rule = {
        let guard = rule_tree.shared_lock.read();
        let rules = child_rules.read_with(&guard);
        rules
            .parse_rule_for_insert(
                &rule_tree.shared_lock,
                rule_text,
                &rule_tree.contents,
                index,
                containing_rule_types,
                parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)?
    };
    {
        let mut guard = rule_tree.shared_lock.write();
        child_rules.write_with(&mut guard).0.insert(index, rule);
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn delete_nested_rule_from_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let child_rules = mutable_child_rules_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        child_rules
            .write_with(&mut guard)
            .remove_rule(index)
            .map_err(CssRuleInsertError::from)?;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn replace_nested_rule_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let child_rules = mutable_child_rules_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let containing_rule_types = CssRuleTypes::from_bits(containing_rule_type_bits);
    let rule = {
        let guard = rule_tree.shared_lock.read();
        let rules = child_rules.read_with(&guard);
        if index >= rules.0.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
        rules
            .parse_rule_for_insert(
                &rule_tree.shared_lock,
                rule_text,
                &rule_tree.contents,
                index,
                containing_rule_types,
                parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)?
    };
    {
        let mut guard = rule_tree.shared_lock.write();
        child_rules.write_with(&mut guard).0[index] = rule;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
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

pub fn insert_keyframe_rule_into_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let rule = Keyframe::parse(rule_text, &rule_tree.contents, &rule_tree.shared_lock)
        .map_err(|_| CssRuleInsertError::Syntax)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        let keyframes_rule = keyframes_rule.write_with(&mut guard);
        if index > keyframes_rule.keyframes.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
        keyframes_rule.keyframes.insert(index, rule);
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn delete_keyframe_rule_from_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        let keyframes_rule = keyframes_rule.write_with(&mut guard);
        if index >= keyframes_rule.keyframes.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
        keyframes_rule.keyframes.remove(index);
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn replace_keyframe_rule_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    {
        let guard = rule_tree.shared_lock.read();
        if index >= keyframes_rule.read_with(&guard).keyframes.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
    }
    let rule = Keyframe::parse(rule_text, &rule_tree.contents, &rule_tree.shared_lock)
        .map_err(|_| CssRuleInsertError::Syntax)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        keyframes_rule.write_with(&mut guard).keyframes[index] = rule;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn set_media_rule_media_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    media_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let media_queries = mutable_media_rule_media_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_media_list_for_rule(media_text)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *media_queries.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_style_rule_declarations_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    declaration_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block = mutable_style_rule_declaration_block_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_declaration_block_for_rule(declaration_text, CssRuleType::Style)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_nested_declarations_rule_declarations_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    declaration_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block = mutable_nested_declarations_rule_block_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_declaration_block_for_rule(declaration_text, CssRuleType::Style)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_keyframe_rule_declarations_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
    declaration_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block =
        mutable_keyframe_rule_declaration_block_for_rule_path(rule_tree, parent_path, index)
            .ok_or(CssRuleInsertError::IndexSize)?;
    let parsed = parse_declaration_block_for_rule(declaration_text, CssRuleType::Keyframe)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn set_keyframe_rule_selector_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
    selector_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframe = mutable_keyframe_for_rule_path(rule_tree, parent_path, index)
        .ok_or(CssRuleInsertError::IndexSize)?;
    let selector = parse_keyframe_selectors(selector_text).ok_or(CssRuleInsertError::Syntax)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        keyframe.write_with(&mut guard).selector = selector;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
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
    ensure_lightmount_rule_tree_prefs();
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

fn parse_stylesheet_rule_tree_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> CssStylesheetRuleTree {
    ensure_lightmount_rule_tree_prefs();
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let contents = StylesheetContents::from_str(
        css_text,
        about_blank_url_data().expect("static about:blank URL should parse"),
        Origin::Author,
        &shared_lock,
        stylesheet_loader,
        None,
        QuirksMode::NoQuirks,
        allow_import_rules,
        None,
    );
    CssStylesheetRuleTree {
        contents,
        shared_lock,
        allow_import_rules,
    }
}

fn refresh_stylesheet_rule_tree_from_css_text(
    rule_tree: &mut CssStylesheetRuleTree,
    css_text: &str,
) {
    let allow_import_rules = rule_tree.allow_import_rules;
    *rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, allow_import_rules);
}

fn mutable_child_rules_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<CssRules>>> {
    let rule = rule_at_path(rule_tree, parent_path)?;
    match rule {
        CssRule::Style(ref style_rule) => {
            {
                let guard = rule_tree.shared_lock.read();
                if let Some(rules) = style_rule.read_with(&guard).rules.clone() {
                    return Some(rules);
                }
            }
            let mut guard = rule_tree.shared_lock.write();
            let style_rule = style_rule.write_with(&mut guard);
            if style_rule.rules.is_none() {
                style_rule.rules = Some(CssRules::new(Vec::new(), &rule_tree.shared_lock));
            }
            style_rule.rules.clone()
        },
        _ => {
            let guard = rule_tree.shared_lock.read();
            existing_child_rules_for_rule(&rule, &guard)
        },
    }
}

fn mutable_keyframes_rule_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<KeyframesRule>>> {
    match rule_at_path(rule_tree, parent_path)? {
        CssRule::Keyframes(rule) => Some(rule),
        _ => None,
    }
}

fn mutable_media_rule_media_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<MediaList>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Media(rule) => Some(rule.media_queries.clone()),
        _ => None,
    }
}

fn mutable_style_rule_declaration_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Style(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(rule.read_with(&guard).block.clone())
        },
        _ => None,
    }
}

fn mutable_nested_declarations_rule_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::NestedDeclarations(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(rule.read_with(&guard).block.clone())
        },
        _ => None,
    }
}

fn mutable_keyframe_rule_declaration_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    let keyframe = mutable_keyframe_for_rule_path(rule_tree, parent_path, index)?;
    let guard = rule_tree.shared_lock.read();
    Some(keyframe.read_with(&guard).block.clone())
}

fn mutable_keyframe_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Option<Arc<crate::shared_lock::Locked<Keyframe>>> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)?;
    let guard = rule_tree.shared_lock.read();
    let keyframes_rule = keyframes_rule.read_with(&guard);
    keyframes_rule.keyframes.get(index).cloned()
}

fn parse_media_list_for_rule(media_text: &str) -> Result<MediaList, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(CssRuleType::Media),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    let mut input = ParserInput::new(media_text);
    let mut input = Parser::new(&mut input);
    Ok(MediaList::parse(&context, &mut input))
}

fn parse_declaration_block_for_rule(
    declaration_text: &str,
    rule_type: CssRuleType,
) -> Result<PropertyDeclarationBlock, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(rule_type),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    let mut input = ParserInput::new(declaration_text);
    let mut input = Parser::new(&mut input);
    Ok(parse_property_declaration_list(&context, &mut input, &[]))
}

fn rule_at_path(rule_tree: &CssStylesheetRuleTree, path: &[usize]) -> Option<CssRule> {
    let guard = rule_tree.shared_lock.read();
    let (first, rest) = path.split_first()?;
    let rules = rule_tree.contents.rules.read_with(&guard);
    let mut rule = rules.0.get(*first)?.clone();
    for index in rest {
        let child_rules = existing_child_rules_for_rule(&rule, &guard)?;
        rule = child_rules.read_with(&guard).0.get(*index)?.clone();
    }
    Some(rule)
}

fn existing_child_rules_for_rule(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<Arc<crate::shared_lock::Locked<CssRules>>> {
    match rule {
        CssRule::Style(rule) => rule.read_with(guard).rules.clone(),
        CssRule::Media(rule) => Some(rule.rules.clone()),
        CssRule::Container(rule) => Some(rule.rules.clone()),
        CssRule::Supports(rule) => Some(rule.rules.clone()),
        CssRule::Page(rule) => Some(rule.read_with(guard).rules.clone()),
        CssRule::Document(rule) => Some(rule.rules.clone()),
        CssRule::LayerBlock(rule) => Some(rule.rules.clone()),
        CssRule::Scope(rule) => Some(rule.rules.clone()),
        CssRule::StartingStyle(rule) => Some(rule.rules.clone()),
        CssRule::AppearanceBase(rule) => Some(rule.rules.clone()),
        _ => None,
    }
}

fn parse_nested_rules_for_mutation(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<ParsedNestedRulesForMutation, CssRuleInsertError> {
    ensure_lightmount_rule_tree_prefs();
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
    ensure_lightmount_rule_tree_prefs();
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

fn nested_rule_tree_mutation_result(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let guard = rule_tree.shared_lock.read();
    let top_rules = rule_tree.contents.rules.read_with(&guard);
    let parent_rule = rule_view_at_path(top_rules.0.as_slice(), parent_path, &guard)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let rules = top_rules
        .0
        .iter()
        .map(|rule| stylesheet_rule_view(rule, &guard))
        .collect::<Vec<_>>();
    Ok(CssNestedRuleMutationResult {
        stylesheet_css_text: css_rule_views_css_text(&rules),
        rules: parent_rule.child_rules.clone(),
        parent_rule,
    })
}

fn rule_view_at_path(
    rules: &[CssRule],
    path: &[usize],
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<CssStylesheetRuleView> {
    let (first, rest) = path.split_first()?;
    let rule = rules.get(*first)?;
    if rest.is_empty() {
        return Some(stylesheet_rule_view(rule, guard));
    }
    rule_view_at_path(rule.children(guard), rest, guard)
}

fn css_rule_views_css_text(rules: &[CssStylesheetRuleView]) -> String {
    rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
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
    ensure_lightmount_rule_tree_prefs();
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

fn ensure_lightmount_rule_tree_prefs() {
    static ENABLE: Once = Once::new();
    ENABLE.call_once(|| {
        static_prefs::set_pref!("layout.css.margin-rules.enabled", true);
    });
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
        delete_keyframe_rule, delete_keyframe_rule_from_stylesheet_rule_tree, delete_nested_rule,
        delete_nested_rule_from_stylesheet_rule_tree, delete_rule_from_stylesheet_rule_tree,
        delete_stylesheet_rule, insert_keyframe_rule,
        insert_keyframe_rule_into_stylesheet_rule_tree, insert_nested_rule,
        insert_nested_rule_into_stylesheet_rule_tree, insert_rule_into_stylesheet_rule_tree,
        insert_stylesheet_rule, keyframe_selector_texts_match, normalize_keyframe_selector_text,
        parse_constructed_stylesheet_rule_texts, parse_constructed_stylesheet_rule_tree,
        parse_stylesheet_rule_for_insert, parse_stylesheet_rule_texts, parse_stylesheet_rule_tree,
        parse_stylesheet_rule_views, replace_keyframe_rule_in_stylesheet_rule_tree,
        replace_nested_rule_in_stylesheet_rule_tree, replace_rule_in_stylesheet_rule_tree,
        serialize_stylesheet, set_keyframe_rule_declarations_in_stylesheet_rule_tree,
        set_keyframe_rule_selector_in_stylesheet_rule_tree,
        set_media_rule_media_in_stylesheet_rule_tree,
        set_nested_declarations_rule_declarations_in_stylesheet_rule_tree,
        set_style_rule_declarations_in_stylesheet_rule_tree, stylesheet_rule_tree_css_text,
        stylesheet_rule_tree_rule_views, CssRuleInsertError,
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
    fn stylesheet_rule_views_include_counter_style_rules() {
        let rules = parse_stylesheet_rule_views(
            r#"@counter-style thumbs { system: cyclic; symbols: "*"; suffix: " "; }"#,
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::CounterStyle);
        assert_eq!(
            rules[0].css_text,
            r#"@counter-style thumbs { system: cyclic; suffix: " "; symbols: "*"; }"#
        );
        assert!(rules[0].child_rules.is_empty());
    }

    #[test]
    fn stylesheet_rule_views_include_font_feature_values_rules() {
        let rules = parse_stylesheet_rule_views(
            "@font-feature-values test_family { @annotation { the_first: 6; } @styleset { yo: 7; di: 10 9 4 5; } }",
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::FontFeatureValues);
        assert_eq!(
            rules[0].css_text,
            "@font-feature-values test_family {\n@annotation {\nthe_first: 6;\n}\n@styleset {\nyo: 7;\ndi: 10 9 4 5;\n}\n}"
        );
        assert!(rules[0].child_rules.is_empty());
    }

    #[test]
    fn stylesheet_rule_views_include_page_rules() {
        let rules = parse_stylesheet_rule_views(
            r#"@page :first { margin-top: 1px; @top-left { content: "x"; color: red; } }"#,
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::Page);
        assert_eq!(
            rules[0].css_text,
            "@page :first {\n  margin-top: 1px;\n  @top-left { content: \"x\"; color: red; }\n}"
        );
        assert_eq!(rules[0].child_rules.len(), 1);
        assert_eq!(rules[0].child_rules[0].rule_type, CssRuleType::Margin);
        assert_eq!(
            rules[0].child_rules[0].css_text,
            "@top-left { content: \"x\"; color: red; }"
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
    fn persistent_stylesheet_rule_tree_mutates_without_reparsing_rule_texts() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .one { color: red; }",
        );

        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .one { color: red; }"
        );
        assert_eq!(
            insert_rule_into_stylesheet_rule_tree(
                &mut rule_tree,
                "@namespace html url(\"http://www.w3.org/1999/xhtml\");",
                2,
            ),
            Err(CssRuleInsertError::InvalidState)
        );

        let inserted = insert_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            "@media screen { svg|path { color: blue; } }",
            2,
        )
        .expect("media rule should insert into existing persistent rule tree");
        assert_eq!(inserted.rules.len(), 3);
        assert_eq!(inserted.rules[2].rule_type, CssRuleType::Media);
        assert_eq!(
            inserted.rules[2].child_rules[0].css_text,
            "svg|path { color: blue; }"
        );
        assert_eq!(
            stylesheet_rule_tree_rule_views(&rule_tree)[2].css_text,
            "@media screen {\n  svg|path { color: blue; }\n}"
        );

        let deleted = delete_rule_from_stylesheet_rule_tree(&mut rule_tree, 1)
            .expect("style rule should delete from same persistent tree");
        assert_eq!(deleted.rules.len(), 2);
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen {\n  svg|path { color: blue; }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_refreshes_namespaces_after_insert() {
        let mut rule_tree = parse_stylesheet_rule_tree("");

        insert_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            "@namespace svg url(\"http://www.w3.org/2000/svg\");",
            0,
        )
        .expect("namespace rule should insert");
        let inserted =
            insert_rule_into_stylesheet_rule_tree(&mut rule_tree, "svg|a { color: white; }", 1)
                .expect("style rule should see namespace inserted into persistent tree");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[1].css_text, "svg|a { color: white; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); svg|a { color: white; }"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_refreshes_namespaces_after_delete() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); svg|a { color: white; }",
        );

        delete_rule_from_stylesheet_rule_tree(&mut rule_tree, 1)
            .expect("style rule should delete before namespace");
        delete_rule_from_stylesheet_rule_tree(&mut rule_tree, 0)
            .expect("namespace rule should delete once no style rules remain");

        assert_eq!(stylesheet_rule_tree_css_text(&rule_tree), "");
        assert_eq!(
            insert_rule_into_stylesheet_rule_tree(&mut rule_tree, "svg|a { color: blue; }", 0),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn persistent_constructed_stylesheet_rule_tree_rejects_import_insert() {
        let mut rule_tree = parse_constructed_stylesheet_rule_tree(".one { color: red; }");

        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".one { color: red; }"
        );
        assert_eq!(
            insert_rule_into_stylesheet_rule_tree(
                &mut rule_tree,
                "@import url(\"ignored.css\");",
                0,
            ),
            Err(CssRuleInsertError::Syntax)
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
    fn persistent_stylesheet_rule_tree_mutates_nested_grouping_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen { .one { color: red; } }",
        );

        let inserted = insert_nested_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[1],
            "svg|path { color: blue; }",
            1,
            CssRuleType::Media.bit(),
            None,
        )
        .expect("nested rule should insert into persistent media rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[1].css_text, "svg|path { color: blue; }");
        assert_eq!(
            inserted.parent_rule.css_text,
            "@media screen {\n  .one { color: red; }\n  svg|path { color: blue; }\n}"
        );
        assert_eq!(
            inserted.stylesheet_css_text,
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen {\n  .one { color: red; }\n  svg|path { color: blue; }\n}"
        );

        let deleted = delete_nested_rule_from_stylesheet_rule_tree(&mut rule_tree, &[1], 0)
            .expect("nested rule should delete from persistent media rule");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(deleted.rules[0].css_text, "svg|path { color: blue; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen {\n  svg|path { color: blue; }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_creates_style_rule_children() {
        let mut rule_tree = parse_stylesheet_rule_tree(".host { color: red; }");

        let inserted = insert_nested_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "> .child { color: blue; }",
            0,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("style rule should get a persistent nested rule list");

        assert_eq!(inserted.rules.len(), 1);
        assert_eq!(inserted.rules[0].css_text, "& > .child { color: blue; }");
        assert_eq!(
            inserted.parent_rule.css_text,
            ".host {\n  color: red;\n  & > .child { color: blue; }\n}"
        );

        let deleted = delete_nested_rule_from_stylesheet_rule_tree(&mut rule_tree, &[0], 0)
            .expect("style rule child should delete");
        assert!(deleted.rules.is_empty());
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".host { color: red; }"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_replaces_top_level_style_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .one { color: red; } .after { color: black; }",
        );

        let mutation = replace_rule_in_stylesheet_rule_tree(
            &mut rule_tree,
            "svg|path { color: blue; & > .icon { opacity: .5; } }",
            1,
        )
        .expect("style rule should replace in persistent tree");

        assert_eq!(mutation.rules.len(), 3);
        assert_eq!(mutation.rules[1].rule_type, CssRuleType::Style);
        assert_eq!(mutation.rules[1].child_rules.len(), 1);
        assert_eq!(
            mutation.rules[1].child_rules[0].css_text,
            "& > .icon { opacity: 0.5; }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); svg|path {\n  color: blue;\n  & > .icon { opacity: 0.5; }\n} .after { color: black; }"
        );
        assert_eq!(
            replace_rule_in_stylesheet_rule_tree(&mut rule_tree, ".missing { color: red; }", 9),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_replaces_nested_style_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            ".host { color: red; & .old { color: blue; } font-size: 12px; }",
        );

        let mutation = replace_nested_rule_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "> .new { color: green; }",
            0,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("nested style rule should replace in persistent tree");

        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "& > .new { color: green; }");
        assert_eq!(mutation.rules[1].css_text, "font-size: 12px;");
        assert_eq!(
            mutation.parent_rule.css_text,
            ".host {\n  color: red;\n  & > .new { color: green; }\n  font-size: 12px;\n}"
        );
        assert_eq!(
            replace_nested_rule_in_stylesheet_rule_tree(
                &mut rule_tree,
                &[0],
                ".missing { color: red; }",
                9,
                CssRuleType::Style.bit(),
                Some(CssRuleType::Style),
            ),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_deep_nested_rule_path() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@media screen { @supports (display: grid) { .one { display: grid; } } }",
        );

        let inserted = insert_nested_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 0],
            ".two { display: contents; }",
            1,
            CssRuleType::Media.bit() | CssRuleType::Supports.bit(),
            None,
        )
        .expect("deep supports rule should mutate in persistent tree");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[1].css_text, ".two { display: contents; }");
        assert_eq!(
            inserted.parent_rule.css_text,
            "@supports (display: grid) {\n  .one { display: grid; }\n  .two { display: contents; }\n}"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@media screen {\n  @supports (display: grid) {\n  .one { display: grid; }\n  .two { display: contents; }\n}\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_media_rule_media_list() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@media screen { @supports (display: grid) { .one { display: grid; } } }",
        );

        let mutation = set_media_rule_media_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "print and (min-width: 10px)",
        )
        .expect("media rule should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Media);
        assert_eq!(
            mutation.parent_rule.css_text,
            "@media print and (min-width: 10px) {\n  @supports (display: grid) {\n  .one { display: grid; }\n}\n}"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@media print and (min-width: 10px) {\n  @supports (display: grid) {\n  .one { display: grid; }\n}\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_style_rule_declarations() {
        let mut rule_tree =
            parse_stylesheet_rule_tree(".host { color: red; & > .child { color: blue; } }");

        let mutation = set_style_rule_declarations_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "margin: 1px 2px; color: green;",
        )
        .expect("style rule declarations should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Style);
        assert_eq!(mutation.parent_rule.child_rules.len(), 1);
        assert_eq!(
            mutation.parent_rule.child_rules[0].css_text,
            "& > .child { color: blue; }"
        );
        assert_eq!(
            mutation.parent_rule.css_text,
            ".host {\n  margin: 1px 2px; color: green;\n  & > .child { color: blue; }\n}"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".host {\n  margin: 1px 2px; color: green;\n  & > .child { color: blue; }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_nested_declarations() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            ".host { & .child { color: blue; } color: red; margin: 0; }",
        );

        let mutation = set_nested_declarations_rule_declarations_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 1],
            "padding: 1px 2px;",
        )
        .expect("nested declarations should update in persistent tree");

        assert_eq!(
            mutation.parent_rule.rule_type,
            CssRuleType::NestedDeclarations
        );
        assert_eq!(mutation.parent_rule.css_text, "padding: 1px 2px;");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".host {\n  & .child { color: blue; }\n  padding: 1px 2px;\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_keyframe_declarations() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@keyframes fade { from { opacity: 0; } to { opacity: 1; } }",
        );

        let mutation = set_keyframe_rule_declarations_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            1,
            "opacity: .5; transform: translateX(10px);",
        )
        .expect("keyframe declarations should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Keyframes);
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(
            mutation.rules[1].css_text,
            "100% { opacity: 0.5; transform: translateX(10px); }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n0% { opacity: 0; }\n100% { opacity: 0.5; transform: translateX(10px); }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_keyframe_selectors() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@keyframes fade { from { opacity: 0; } to { opacity: 1; } }",
        );

        let mutation =
            set_keyframe_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], 1, "75%, to")
                .expect("keyframe selector should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Keyframes);
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(mutation.rules[1].css_text, "75%, 100% { opacity: 1; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n0% { opacity: 0; }\n75%, 100% { opacity: 1; }\n}"
        );
        assert_eq!(
            set_keyframe_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], 9, "50%"),
            Err(CssRuleInsertError::IndexSize)
        );
        assert_eq!(
            set_keyframe_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], 0, "body"),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_replaces_keyframe_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@keyframes fade { from { opacity: 0; } to { opacity: 1; } }",
        );

        let mutation = replace_keyframe_rule_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "80% { opacity: .8; transform: scale(1); }",
            1,
        )
        .expect("keyframe child should replace in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Keyframes);
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(
            mutation.rules[1].css_text,
            "80% { opacity: 0.8; transform: scale(1); }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n0% { opacity: 0; }\n80% { opacity: 0.8; transform: scale(1); }\n}"
        );
        assert_eq!(
            replace_keyframe_rule_in_stylesheet_rule_tree(
                &mut rule_tree,
                &[0],
                "body { opacity: 0; }",
                1,
            ),
            Err(CssRuleInsertError::Syntax)
        );
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
    fn persistent_stylesheet_rule_tree_mutates_keyframes_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree("@keyframes fade { from { opacity: 0; } }");

        let inserted = insert_keyframe_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "to { opacity: 1; transform: translateX(10px); }",
            1,
        )
        .expect("keyframe rule should insert into persistent keyframes rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(
            inserted.rules[1].css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );
        assert_eq!(
            inserted.parent_rule.css_text,
            "@keyframes fade {\n0% { opacity: 0; }\n100% { opacity: 1; transform: translateX(10px); }\n}"
        );
        assert_eq!(inserted.stylesheet_css_text, inserted.parent_rule.css_text);

        let deleted = delete_keyframe_rule_from_stylesheet_rule_tree(&mut rule_tree, &[0], 0)
            .expect("keyframe rule should delete from persistent keyframes rule");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(
            deleted.rules[0].css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n100% { opacity: 1; transform: translateX(10px); }\n}"
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
