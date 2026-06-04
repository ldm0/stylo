/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Lightmount-facing selector invalidation summaries.
//!
//! This module exposes a small, read-only view over Stylo's existing selector
//! invalidation maps. It lets Lightmount consume selector dependency truth
//! without adopting Servo's full restyle lifecycle.

use crate::invalidation::element::invalidation_map::{
    AdditionalRelativeSelectorInvalidationMap, Dependency, DependencyInvalidationKind,
    InvalidationMap, NormalDependencyInvalidationKind, RelativeDependencyInvalidationKind,
    StateDependency,
};
use crate::selector_map::SelectorMap;
use crate::values::AtomIdent;
use crate::{Atom, LocalName};
use dom::ElementState;

/// Sibling-sensitive selector dependencies exposed for Lightmount's style
/// invalidation summary.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountSiblingInvalidationSummary {
    /// Class tokens whose mutation can affect sibling selector invalidation.
    pub class_dependencies: Vec<Atom>,
    /// IDs whose mutation can affect sibling selector invalidation.
    pub id_dependencies: Vec<Atom>,
    /// Attribute local names whose mutation can affect sibling selector
    /// invalidation.
    pub attribute_dependencies: Vec<LocalName>,
    /// Whether focus-like state changes can affect sibling selector
    /// invalidation.
    pub focus_dependency: bool,
    /// Whether :target state changes can affect sibling selector invalidation.
    pub target_dependency: bool,
    /// Whether there are sibling-sensitive dependencies that are not covered by
    /// the class/id/attribute/focus/target keys above.
    pub unknown_dependency: bool,
}

impl LightmountSiblingInvalidationSummary {
    /// Returns whether any sibling-sensitive dependency was found.
    #[inline]
    pub fn has_any_dependency(&self) -> bool {
        !self.class_dependencies.is_empty()
            || !self.id_dependencies.is_empty()
            || !self.attribute_dependencies.is_empty()
            || self.focus_dependency
            || self.target_dependency
            || self.unknown_dependency
    }

    fn note_state_dependency(&mut self, state: ElementState) {
        let mut known = false;
        if state
            .intersects(ElementState::FOCUS | ElementState::FOCUSRING | ElementState::FOCUS_WITHIN)
        {
            self.focus_dependency = true;
            known = true;
        }
        if state.intersects(ElementState::URLTARGET) {
            self.target_dependency = true;
            known = true;
        }
        if !known {
            self.unknown_dependency = true;
        }
    }

    fn note_unknown_dependency(&mut self) {
        self.unknown_dependency = true;
    }

    /// Merge another sibling invalidation summary into this one.
    pub fn extend(&mut self, other: LightmountSiblingInvalidationSummary) {
        self.class_dependencies.extend(other.class_dependencies);
        self.id_dependencies.extend(other.id_dependencies);
        self.attribute_dependencies
            .extend(other.attribute_dependencies);
        self.focus_dependency |= other.focus_dependency;
        self.target_dependency |= other.target_dependency;
        self.unknown_dependency |= other.unknown_dependency;
    }

    /// Returns whether mutating this class token can affect sibling selector
    /// invalidation.
    #[inline]
    pub fn has_class_dependency(&self, class: &Atom) -> bool {
        self.class_dependencies
            .iter()
            .any(|candidate| candidate == class)
    }

    /// Returns whether mutating this id can affect sibling selector
    /// invalidation.
    #[inline]
    pub fn has_id_dependency(&self, id: &Atom) -> bool {
        self.id_dependencies.iter().any(|candidate| candidate == id)
    }

    /// Returns whether mutating this attribute can affect sibling selector
    /// invalidation.
    #[inline]
    pub fn has_attribute_dependency(&self, attribute: &LocalName) -> bool {
        self.attribute_dependencies
            .iter()
            .any(|candidate| candidate == attribute)
    }
}

/// Lightmount-facing invalidation dependency kind extracted from Stylo's
/// selector invalidation maps.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountDependencyKind {
    /// The changed element itself can be invalidated.
    Element,
    /// The changed element and descendants can be invalidated.
    ElementAndDescendants,
    /// Descendants of the changed element can be invalidated.
    Descendants,
    /// Following siblings of the changed element can be invalidated.
    Siblings,
    /// Slotted elements can be invalidated.
    SlottedElements,
    /// Exposed parts can be invalidated.
    Parts,
    /// Relative selector anchors among ancestors can be invalidated.
    RelativeAncestors,
    /// A relative selector parent anchor can be invalidated.
    RelativeParent,
    /// A relative selector previous-sibling anchor can be invalidated.
    RelativePrevSibling,
    /// Relative selector ancestor previous-sibling anchors can be invalidated.
    RelativeAncestorPrevSibling,
    /// Relative selector earlier-sibling anchors can be invalidated.
    RelativeEarlierSibling,
    /// Relative selector ancestor earlier-sibling anchors can be invalidated.
    RelativeAncestorEarlierSibling,
    /// Scope dependencies can be invalidated.
    Scope,
}

/// Reason a dependency query cannot be represented as exact dependency kinds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountDependencyFallbackReason {
    /// Stylo recorded an unknown dependency for this query.
    UnknownDependency,
    /// Stylo's invalidation map requires full-selector invalidation.
    FullSelector,
    /// Relative selector dependencies are present but not exposed precisely.
    RelativeAnySelector,
    /// Scope dependencies are present but not exposed precisely.
    ScopeDependency,
    /// State dependencies are present but not exposed precisely.
    UnsupportedStateDependency,
    /// Shadow dependency exactness could not be represented by this query.
    UnsupportedShadowDependency,
    /// The dependency shape cannot currently be represented by an exact retained
    /// invalidator path.
    UnsupportedDependency,
    /// `:nth-child(... of ...)` dependency exactness could not be represented
    /// by the retained invalidator's current root set.
    NthOfDependency,
    /// A selector-list dependency nested inside a relative selector cannot be
    /// represented by the retained invalidator's exact root set.
    NestedRelativeSelectorDependency,
}

/// Which fallback roots may be used when a dependency query is not exact.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightmountDependencyFallbackRootPolicy {
    /// Mutation-context roots are sufficient as the conservative cleanup target.
    ContextRoots,
    /// The caller must use source-local or source-scope fallback roots.
    SourceFallback,
}

/// Dependency root categories needed by Lightmount's DOM-backed fallback-root
/// construction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LightmountDependencyContextRootFlags {
    /// The query cannot be covered by context roots and needs source fallback.
    pub requires_source_fallback: bool,
    /// The changed element's local subtree can be affected.
    pub local_subtree: bool,
    /// Ancestors of the changed element can be affected.
    pub ancestor_chain: bool,
    /// Following siblings of the changed element can be affected.
    pub following_siblings: bool,
    /// The query includes a direct previous-sibling relative dependency.
    pub direct_previous_sibling_relative: bool,
    /// The previous element sibling can be affected.
    pub previous_sibling: bool,
    /// Earlier element siblings can be affected.
    pub earlier_siblings: bool,
    /// Previous siblings of ancestors can be affected.
    pub ancestor_previous_siblings: bool,
    /// Earlier siblings of ancestors can be affected.
    pub ancestor_earlier_siblings: bool,
    /// Assigned elements matched by `::slotted(...)` can be affected.
    pub slotted_elements: bool,
    /// Exposed part elements matched by `::part(...)` can be affected.
    pub parts: bool,
}

/// Conservative query result for one changed class/id/attribute/state token.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountDependencyQueryResult {
    kinds: Vec<LightmountDependencyKind>,
    unknown_dependency: bool,
    fallback_reasons: Vec<LightmountDependencyFallbackReason>,
}

impl LightmountDependencyQueryResult {
    fn add_kind(&mut self, kind: LightmountDependencyKind) {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
    }

    fn add_fallback_reason(&mut self, reason: LightmountDependencyFallbackReason) {
        if !self.fallback_reasons.contains(&reason) {
            self.fallback_reasons.push(reason);
        }
        if matches!(
            reason,
            LightmountDependencyFallbackReason::UnknownDependency
        ) {
            self.unknown_dependency = true;
        }
    }

    fn extend(&mut self, other: Self) {
        for kind in other.kinds {
            self.add_kind(kind);
        }
        self.unknown_dependency |= other.unknown_dependency;
        for reason in other.fallback_reasons {
            self.add_fallback_reason(reason);
        }
        if other.unknown_dependency {
            self.add_fallback_reason(LightmountDependencyFallbackReason::UnknownDependency);
        }
    }

    /// Returns whether any dependency matched the query.
    #[inline]
    pub fn has_any_dependency(&self) -> bool {
        self.unknown_dependency || !self.kinds.is_empty() || !self.fallback_reasons.is_empty()
    }

    /// Returns the concrete dependency kinds captured for this query.
    #[inline]
    pub fn kinds(&self) -> &[LightmountDependencyKind] {
        &self.kinds
    }

    /// Returns conservative fallback reasons captured for this query.
    #[inline]
    pub fn fallback_reasons(&self) -> &[LightmountDependencyFallbackReason] {
        &self.fallback_reasons
    }

    /// Returns whether this query requires conservative fallback handling.
    #[inline]
    pub fn requires_fallback(&self) -> bool {
        !self.fallback_reasons.is_empty()
    }

    /// Returns the fallback-root policy for this dependency query.
    #[inline]
    pub fn fallback_root_policy(&self) -> LightmountDependencyFallbackRootPolicy {
        if !self.fallback_reasons.is_empty()
            && self.fallback_reasons.iter().all(|reason| {
                matches!(
                    reason,
                    LightmountDependencyFallbackReason::NestedRelativeSelectorDependency
                        | LightmountDependencyFallbackReason::NthOfDependency
                )
            })
        {
            LightmountDependencyFallbackRootPolicy::ContextRoots
        } else {
            LightmountDependencyFallbackRootPolicy::SourceFallback
        }
    }

    /// Returns explicit fallback reasons, or conservative shape-derived reasons
    /// when the caller has already determined this query needs fallback handling.
    #[inline]
    pub fn fallback_or_shape_reasons(&self) -> Vec<LightmountDependencyFallbackReason> {
        if !self.fallback_reasons.is_empty() {
            return self.fallback_reasons.clone();
        }
        if self.kinds.contains(&LightmountDependencyKind::Scope) {
            return vec![LightmountDependencyFallbackReason::ScopeDependency];
        }
        vec![LightmountDependencyFallbackReason::UnsupportedDependency]
    }

    /// Returns whether the query can affect following sibling invalidation.
    #[inline]
    pub fn has_sibling_dependency(&self) -> bool {
        self.unknown_dependency
            || self.kinds.iter().any(|kind| {
                matches!(
                    kind,
                    LightmountDependencyKind::Siblings
                        | LightmountDependencyKind::RelativePrevSibling
                        | LightmountDependencyKind::RelativeAncestorPrevSibling
                        | LightmountDependencyKind::RelativeEarlierSibling
                        | LightmountDependencyKind::RelativeAncestorEarlierSibling
                )
            })
    }

    /// Returns whether this query can affect relative selector anchors.
    #[inline]
    pub fn has_relative_selector_dependency(&self) -> bool {
        self.kinds.iter().any(|kind| {
            matches!(
                kind,
                LightmountDependencyKind::RelativeAncestors
                    | LightmountDependencyKind::RelativeParent
                    | LightmountDependencyKind::RelativePrevSibling
                    | LightmountDependencyKind::RelativeEarlierSibling
                    | LightmountDependencyKind::RelativeAncestorPrevSibling
                    | LightmountDependencyKind::RelativeAncestorEarlierSibling
            )
        })
    }

    /// Returns whether this query can affect previous-sibling relative selector
    /// anchors.
    #[inline]
    pub fn has_relative_previous_sibling_dependency(&self) -> bool {
        self.kinds.iter().any(|kind| {
            matches!(
                kind,
                LightmountDependencyKind::RelativePrevSibling
                    | LightmountDependencyKind::RelativeEarlierSibling
                    | LightmountDependencyKind::RelativeAncestorPrevSibling
                    | LightmountDependencyKind::RelativeAncestorEarlierSibling
            )
        })
    }

    /// Returns whether this query is limited to a direct previous-sibling
    /// relative dependency plus the following-sibling dependency Stylo records
    /// for `+`.
    #[inline]
    pub fn has_only_direct_relative_previous_sibling_dependency(&self) -> bool {
        !self.kinds.is_empty()
            && self.kinds.iter().all(|kind| {
                matches!(
                    kind,
                    LightmountDependencyKind::RelativePrevSibling
                        | LightmountDependencyKind::Siblings
                )
            })
            && self
                .kinds
                .contains(&LightmountDependencyKind::RelativePrevSibling)
    }

    /// Returns whether this query can affect `::slotted(...)` invalidation.
    #[inline]
    pub fn has_slotted_dependency(&self) -> bool {
        self.unknown_dependency
            || self
                .kinds
                .iter()
                .any(|kind| matches!(kind, LightmountDependencyKind::SlottedElements))
    }

    /// Returns the fallback-root categories this query can affect.
    #[inline]
    pub fn context_root_flags(&self) -> LightmountDependencyContextRootFlags {
        let mut flags = LightmountDependencyContextRootFlags {
            requires_source_fallback: self.requires_fallback(),
            ..LightmountDependencyContextRootFlags::default()
        };
        for kind in &self.kinds {
            match kind {
                LightmountDependencyKind::Element
                | LightmountDependencyKind::ElementAndDescendants
                | LightmountDependencyKind::Descendants => {
                    flags.local_subtree = true;
                },
                LightmountDependencyKind::Siblings => {
                    flags.following_siblings = true;
                },
                LightmountDependencyKind::SlottedElements => {
                    flags.slotted_elements = true;
                },
                LightmountDependencyKind::Parts => {
                    flags.parts = true;
                },
                LightmountDependencyKind::RelativeAncestors
                | LightmountDependencyKind::RelativeParent => {
                    flags.ancestor_chain = true;
                },
                LightmountDependencyKind::RelativePrevSibling => {
                    flags.direct_previous_sibling_relative = true;
                    flags.previous_sibling = true;
                },
                LightmountDependencyKind::RelativeEarlierSibling => {
                    flags.earlier_siblings = true;
                },
                LightmountDependencyKind::RelativeAncestorPrevSibling => {
                    flags.ancestor_chain = true;
                    flags.ancestor_previous_siblings = true;
                },
                LightmountDependencyKind::RelativeAncestorEarlierSibling => {
                    flags.ancestor_chain = true;
                    flags.ancestor_earlier_siblings = true;
                },
                LightmountDependencyKind::Scope => {
                    flags.requires_source_fallback = true;
                },
            }
        }
        flags
    }
}

/// Keyed dependency query summary exposed for Lightmount.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountDependencyInvalidationSummary {
    class_dependencies: Vec<(Atom, LightmountDependencyQueryResult)>,
    id_dependencies: Vec<(Atom, LightmountDependencyQueryResult)>,
    type_dependencies: Vec<(LocalName, LightmountDependencyQueryResult)>,
    universal_dependency: LightmountDependencyQueryResult,
    attribute_dependencies: Vec<(LocalName, LightmountDependencyQueryResult)>,
    custom_state_dependencies: Vec<(AtomIdent, LightmountDependencyQueryResult)>,
    state_dependencies: Vec<(u64, LightmountDependencyQueryResult)>,
    unknown_state_dependency_bits: u64,
    focus_dependency: LightmountDependencyQueryResult,
    focus_within_dependency: LightmountDependencyQueryResult,
    target_dependency: LightmountDependencyQueryResult,
    unknown_dependency: bool,
}

impl LightmountDependencyInvalidationSummary {
    fn note_class_dependency(&mut self, class: Atom, result: LightmountDependencyQueryResult) {
        lightmount_note_keyed_dependency(&mut self.class_dependencies, class, result);
    }

    fn note_id_dependency(&mut self, id: Atom, result: LightmountDependencyQueryResult) {
        lightmount_note_keyed_dependency(&mut self.id_dependencies, id, result);
    }

    fn note_attribute_dependency(
        &mut self,
        attribute: LocalName,
        result: LightmountDependencyQueryResult,
    ) {
        lightmount_note_keyed_dependency(&mut self.attribute_dependencies, attribute, result);
    }

    fn note_type_dependency(
        &mut self,
        local_name: LocalName,
        result: LightmountDependencyQueryResult,
    ) {
        lightmount_note_keyed_dependency(&mut self.type_dependencies, local_name, result);
    }

    fn note_universal_dependency(&mut self, result: LightmountDependencyQueryResult) {
        self.universal_dependency.extend(result);
    }

    fn note_state_dependency(
        &mut self,
        state: ElementState,
        result: LightmountDependencyQueryResult,
    ) {
        if state.is_empty() {
            self.unknown_dependency = true;
            return;
        }
        lightmount_note_keyed_dependency(
            &mut self.state_dependencies,
            state.bits(),
            result.clone(),
        );
        if state.intersects(ElementState::FOCUS | ElementState::FOCUSRING) {
            self.focus_dependency.extend(result.clone());
        }
        if state.intersects(ElementState::FOCUS_WITHIN) {
            self.focus_dependency.extend(result.clone());
            self.focus_within_dependency.extend(result.clone());
        }
        if state.intersects(ElementState::URLTARGET) {
            self.target_dependency.extend(result);
        }
    }

    fn note_custom_state_dependency(
        &mut self,
        state: AtomIdent,
        result: LightmountDependencyQueryResult,
    ) {
        lightmount_note_keyed_dependency(&mut self.custom_state_dependencies, state, result);
    }

    pub(crate) fn note_nth_of_class_dependency(&mut self, class: Atom) {
        self.note_class_dependency(class, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_nth_of_id_dependency(&mut self, id: Atom) {
        self.note_id_dependency(id, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_nth_of_attribute_dependency(&mut self, attribute: LocalName) {
        self.note_attribute_dependency(attribute, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_nth_of_state_dependency(&mut self, state: ElementState) {
        if state.is_empty() {
            return;
        }
        self.note_state_dependency(state, lightmount_nth_of_dependency_query_result());
    }

    pub(crate) fn note_unrepresented_state_dependencies(&mut self, state: ElementState) {
        if state.is_empty() {
            return;
        }
        let represented_bits = self
            .state_dependencies
            .iter()
            .fold(0, |bits, (state_bits, _)| bits | *state_bits);
        self.unknown_state_dependency_bits |= state.bits() & !represented_bits;
    }

    fn mark_unknown_dependency(&mut self) {
        self.unknown_dependency = true;
    }

    pub(crate) fn extend(&mut self, other: Self) {
        for (class, result) in other.class_dependencies {
            self.note_class_dependency(class, result);
        }
        for (id, result) in other.id_dependencies {
            self.note_id_dependency(id, result);
        }
        for (attribute, result) in other.attribute_dependencies {
            self.note_attribute_dependency(attribute, result);
        }
        for (local_name, result) in other.type_dependencies {
            self.note_type_dependency(local_name, result);
        }
        for (state, result) in other.custom_state_dependencies {
            self.note_custom_state_dependency(state, result);
        }
        self.universal_dependency.extend(other.universal_dependency);
        for (state_bits, result) in other.state_dependencies {
            lightmount_note_keyed_dependency(&mut self.state_dependencies, state_bits, result);
        }
        self.unknown_state_dependency_bits |= other.unknown_state_dependency_bits;
        self.focus_dependency.extend(other.focus_dependency);
        self.focus_within_dependency
            .extend(other.focus_within_dependency);
        self.target_dependency.extend(other.target_dependency);
        self.unknown_dependency |= other.unknown_dependency;
    }

    /// Query dependencies for a changed class token.
    pub fn query_class(&self, class: &Atom) -> LightmountDependencyQueryResult {
        self.class_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == class).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for a changed id.
    pub fn query_id(&self, id: &Atom) -> LightmountDependencyQueryResult {
        self.id_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == id).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for a changed attribute.
    pub fn query_attribute(&self, attribute: &LocalName) -> LightmountDependencyQueryResult {
        self.attribute_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == attribute).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for an inserted or removed element local name.
    pub fn query_type(&self, local_name: &LocalName) -> LightmountDependencyQueryResult {
        self.type_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == local_name).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for an inserted or removed element matching `*`.
    pub fn query_universal(&self) -> LightmountDependencyQueryResult {
        self.universal_dependency.clone()
    }

    /// Query dependencies for a changed element state bitset.
    pub fn query_state(&self, state: ElementState) -> LightmountDependencyQueryResult {
        let mut result = LightmountDependencyQueryResult::default();
        let bits = state.bits();
        for (candidate_bits, candidate_result) in &self.state_dependencies {
            if candidate_bits & bits != 0 {
                result.extend(candidate_result.clone());
            }
        }
        if self.unknown_state_dependency_bits & bits != 0 {
            result.add_fallback_reason(
                LightmountDependencyFallbackReason::UnsupportedStateDependency,
            );
        }
        result
    }

    /// Query dependencies for a changed CSS custom state.
    pub fn query_custom_state(&self, state: &AtomIdent) -> LightmountDependencyQueryResult {
        self.custom_state_dependencies
            .iter()
            .find_map(|(candidate, result)| (candidate == state).then(|| result.clone()))
            .unwrap_or_default()
    }

    /// Query dependencies for focus-like state changes.
    pub fn query_focus(&self) -> LightmountDependencyQueryResult {
        self.focus_dependency.clone()
    }

    /// Query dependencies for :focus-within state changes.
    pub fn query_focus_within(&self) -> LightmountDependencyQueryResult {
        self.focus_within_dependency.clone()
    }

    /// Query dependencies for :target state changes.
    pub fn query_target(&self) -> LightmountDependencyQueryResult {
        self.target_dependency.clone()
    }

    /// Returns whether any unkeyed dependency was found.
    #[inline]
    pub fn has_unknown_dependency(&self) -> bool {
        self.unknown_dependency
    }

    /// Returns whether any known dependency can affect following siblings.
    #[inline]
    pub fn has_sibling_dependency(&self) -> bool {
        self.unknown_dependency
            || self.unknown_state_dependency_bits != 0
            || self
                .class_dependencies
                .iter()
                .chain(self.id_dependencies.iter())
                .map(|(_, result)| result)
                .chain(self.attribute_dependencies.iter().map(|(_, result)| result))
                .chain(
                    self.custom_state_dependencies
                        .iter()
                        .map(|(_, result)| result),
                )
                .chain(self.state_dependencies.iter().map(|(_, result)| result))
                .chain(std::iter::once(&self.focus_dependency))
                .chain(std::iter::once(&self.focus_within_dependency))
                .chain(std::iter::once(&self.target_dependency))
                .chain(std::iter::once(&self.universal_dependency))
                .any(LightmountDependencyQueryResult::has_sibling_dependency)
            || self
                .type_dependencies
                .iter()
                .map(|(_, result)| result)
                .any(LightmountDependencyQueryResult::has_sibling_dependency)
    }

    /// Returns whether any known dependency can affect `::slotted(...)`
    /// invalidation.
    #[inline]
    pub fn has_slotted_dependency(&self) -> bool {
        self.unknown_dependency
            || self.unknown_state_dependency_bits != 0
            || self
                .class_dependencies
                .iter()
                .chain(self.id_dependencies.iter())
                .map(|(_, result)| result)
                .chain(self.attribute_dependencies.iter().map(|(_, result)| result))
                .chain(
                    self.custom_state_dependencies
                        .iter()
                        .map(|(_, result)| result),
                )
                .chain(self.state_dependencies.iter().map(|(_, result)| result))
                .chain(std::iter::once(&self.focus_dependency))
                .chain(std::iter::once(&self.focus_within_dependency))
                .chain(std::iter::once(&self.target_dependency))
                .chain(std::iter::once(&self.universal_dependency))
                .any(LightmountDependencyQueryResult::has_slotted_dependency)
    }
}

fn lightmount_nth_of_dependency_query_result() -> LightmountDependencyQueryResult {
    let mut result = LightmountDependencyQueryResult::default();
    result.add_kind(LightmountDependencyKind::Siblings);
    result.add_fallback_reason(LightmountDependencyFallbackReason::NthOfDependency);
    result
}

fn lightmount_note_keyed_dependency<K: Eq>(
    dependencies: &mut Vec<(K, LightmountDependencyQueryResult)>,
    key: K,
    result: LightmountDependencyQueryResult,
) {
    if !result.has_any_dependency() {
        return;
    }
    if let Some((_, existing)) = dependencies
        .iter_mut()
        .find(|(candidate, _)| candidate == &key)
    {
        existing.extend(result);
        return;
    }
    dependencies.push((key, result));
}

pub(crate) fn lightmount_sibling_summary_for_invalidation_map(
    map: &InvalidationMap,
) -> LightmountSiblingInvalidationSummary {
    let mut summary = LightmountSiblingInvalidationSummary::default();
    if map.unkeyed_sibling_dependency {
        summary.note_unknown_dependency();
    }
    if lightmount_dependencies_have_sibling_sensitive(&map.any_to_selector) {
        summary.note_unknown_dependency();
    }
    for (class, dependencies) in map.class_to_selector.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.class_dependencies.push(class.clone());
        }
    }
    for (id, dependencies) in map.id_to_selector.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.id_dependencies.push(id.clone());
        }
    }
    for (attribute, dependencies) in map.other_attribute_affecting_selectors.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.attribute_dependencies.push(attribute.clone());
        }
    }
    for (_, dependencies) in map.custom_state_affecting_selectors.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.note_unknown_dependency();
        }
    }
    lightmount_collect_state_sibling_dependencies_from_selector_map(
        &map.state_affecting_selectors,
        &mut summary,
    );
    summary
}

pub(crate) fn lightmount_sibling_summary_for_relative_invalidation_map(
    map: &AdditionalRelativeSelectorInvalidationMap,
) -> LightmountSiblingInvalidationSummary {
    let mut summary = LightmountSiblingInvalidationSummary::default();
    if map.needs_ancestors_traversal {
        summary.note_unknown_dependency();
    }
    for dependency in &map.any_to_selector {
        if lightmount_dependency_is_sibling_sensitive(dependency) {
            summary.note_unknown_dependency();
        }
    }
    for (_, dependencies) in map.type_to_selector.iter() {
        if lightmount_dependencies_have_sibling_sensitive(dependencies) {
            summary.note_unknown_dependency();
        }
    }
    if lightmount_selector_map_has_sibling_dependency(&map.ts_state_to_selector, |dependency| {
        &dependency.dep
    }) {
        summary.note_unknown_dependency();
    }
    summary
}

pub(crate) fn lightmount_dependency_summary_for_invalidation_map(
    map: &InvalidationMap,
) -> LightmountDependencyInvalidationSummary {
    let mut summary = LightmountDependencyInvalidationSummary::default();
    if map.unkeyed_sibling_dependency {
        summary.mark_unknown_dependency();
    }
    summary.note_universal_dependency(lightmount_dependency_query_result_for_dependencies(
        &map.any_to_selector,
    ));
    for (class, dependencies) in map.class_to_selector.iter() {
        summary.note_class_dependency(
            class.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (id, dependencies) in map.id_to_selector.iter() {
        summary.note_id_dependency(
            id.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (attribute, dependencies) in map.other_attribute_affecting_selectors.iter() {
        summary.note_attribute_dependency(
            attribute.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (local_name, dependencies) in map.type_to_selector.iter() {
        summary.note_type_dependency(
            local_name.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    for (state, dependencies) in map.custom_state_affecting_selectors.iter() {
        summary.note_custom_state_dependency(
            state.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    lightmount_collect_state_dependencies_from_selector_map(
        &map.state_affecting_selectors,
        &mut summary,
    );
    summary
}

pub(crate) fn lightmount_dependency_summary_for_relative_invalidation_map(
    map: &AdditionalRelativeSelectorInvalidationMap,
) -> LightmountDependencyInvalidationSummary {
    let mut summary = LightmountDependencyInvalidationSummary::default();
    if map.needs_ancestors_traversal {
        summary.mark_unknown_dependency();
    }
    summary.note_universal_dependency(lightmount_dependency_query_result_for_dependencies(
        &map.any_to_selector,
    ));
    for (local_name, dependencies) in map.type_to_selector.iter() {
        summary.note_type_dependency(
            local_name.clone(),
            lightmount_dependency_query_result_for_dependencies(dependencies),
        );
    }
    if !map.ts_state_to_selector.is_empty() {
        summary.mark_unknown_dependency();
    }
    summary
}

fn lightmount_dependency_query_result_for_dependencies(
    dependencies: &[Dependency],
) -> LightmountDependencyQueryResult {
    let mut result = LightmountDependencyQueryResult::default();
    for dependency in dependencies {
        lightmount_collect_dependency_query_result(dependency, &mut result);
    }
    result
}

fn lightmount_collect_dependency_query_result(
    dependency: &Dependency,
    result: &mut LightmountDependencyQueryResult,
) {
    match dependency.invalidation_kind() {
        DependencyInvalidationKind::FullSelector => {
            result.add_fallback_reason(LightmountDependencyFallbackReason::FullSelector);
        },
        DependencyInvalidationKind::Normal(kind) => {
            result.add_kind(match kind {
                NormalDependencyInvalidationKind::Element => LightmountDependencyKind::Element,
                NormalDependencyInvalidationKind::ElementAndDescendants => {
                    LightmountDependencyKind::ElementAndDescendants
                },
                NormalDependencyInvalidationKind::Descendants => {
                    LightmountDependencyKind::Descendants
                },
                NormalDependencyInvalidationKind::Siblings => LightmountDependencyKind::Siblings,
                NormalDependencyInvalidationKind::SlottedElements => {
                    LightmountDependencyKind::SlottedElements
                },
                NormalDependencyInvalidationKind::Parts => LightmountDependencyKind::Parts,
            });
        },
        DependencyInvalidationKind::Relative(kind) => {
            result.add_kind(match kind {
                RelativeDependencyInvalidationKind::Ancestors => {
                    LightmountDependencyKind::RelativeAncestors
                },
                RelativeDependencyInvalidationKind::Parent => {
                    LightmountDependencyKind::RelativeParent
                },
                RelativeDependencyInvalidationKind::PrevSibling => {
                    LightmountDependencyKind::RelativePrevSibling
                },
                RelativeDependencyInvalidationKind::AncestorPrevSibling => {
                    LightmountDependencyKind::RelativeAncestorPrevSibling
                },
                RelativeDependencyInvalidationKind::EarlierSibling => {
                    LightmountDependencyKind::RelativeEarlierSibling
                },
                RelativeDependencyInvalidationKind::AncestorEarlierSibling => {
                    LightmountDependencyKind::RelativeAncestorEarlierSibling
                },
            });
        },
        DependencyInvalidationKind::Scope(_) => {
            result.add_kind(LightmountDependencyKind::Scope);
        },
    }
    if dependency.right_combinator_is_next_sibling()
        || dependency.dependency_is_relative_with_single_next_sibling()
    {
        result.add_kind(LightmountDependencyKind::Siblings);
    }
    if lightmount_dependency_has_nested_relative_dependency(dependency) {
        result.add_fallback_reason(
            LightmountDependencyFallbackReason::NestedRelativeSelectorDependency,
        );
    }
    if let Some(next) = dependency.next.as_ref() {
        for dependency in next.slice() {
            lightmount_collect_dependency_query_result(dependency, result);
        }
    }
}

fn lightmount_dependency_has_nested_relative_dependency(dependency: &Dependency) -> bool {
    if matches!(
        dependency.invalidation_kind(),
        DependencyInvalidationKind::Relative(_)
    ) {
        return false;
    }
    dependency
        .next
        .as_ref()
        .is_some_and(|next| lightmount_dependency_chain_contains_relative_dependency(next.slice()))
}

fn lightmount_dependency_chain_contains_relative_dependency(dependencies: &[Dependency]) -> bool {
    dependencies.iter().any(|dependency| {
        matches!(
            dependency.invalidation_kind(),
            DependencyInvalidationKind::Relative(_)
        ) || dependency.next.as_ref().is_some_and(|next| {
            lightmount_dependency_chain_contains_relative_dependency(next.slice())
        })
    })
}

fn lightmount_dependencies_have_sibling_sensitive(dependencies: &[Dependency]) -> bool {
    dependencies
        .iter()
        .any(lightmount_dependency_is_sibling_sensitive)
}

fn lightmount_collect_state_dependencies_from_selector_map(
    map: &SelectorMap<StateDependency>,
    summary: &mut LightmountDependencyInvalidationSummary,
) {
    for dependency in &map.root {
        lightmount_collect_state_dependency(dependency, summary);
    }
    for (_, dependencies) in map.id_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.class_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.attribute_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.local_name_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.namespace_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_dependency(dependency, summary);
        }
    }
    for dependency in &map.rare_pseudo_classes {
        lightmount_collect_state_dependency(dependency, summary);
    }
    for dependency in &map.other {
        lightmount_collect_state_dependency(dependency, summary);
    }
}

fn lightmount_collect_state_dependency(
    dependency: &StateDependency,
    summary: &mut LightmountDependencyInvalidationSummary,
) {
    summary.note_state_dependency(
        dependency.state,
        lightmount_dependency_query_result_for_dependencies(std::slice::from_ref(&dependency.dep)),
    );
}

fn lightmount_collect_state_sibling_dependencies_from_selector_map(
    map: &SelectorMap<StateDependency>,
    summary: &mut LightmountSiblingInvalidationSummary,
) {
    for dependency in &map.root {
        lightmount_collect_state_sibling_dependency(dependency, summary);
    }
    for (_, dependencies) in map.id_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.class_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.attribute_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.local_name_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for (_, dependencies) in map.namespace_hash.iter() {
        for dependency in dependencies {
            lightmount_collect_state_sibling_dependency(dependency, summary);
        }
    }
    for dependency in &map.rare_pseudo_classes {
        lightmount_collect_state_sibling_dependency(dependency, summary);
    }
    for dependency in &map.other {
        lightmount_collect_state_sibling_dependency(dependency, summary);
    }
}

fn lightmount_collect_state_sibling_dependency(
    dependency: &StateDependency,
    summary: &mut LightmountSiblingInvalidationSummary,
) {
    if lightmount_dependency_is_sibling_sensitive(&dependency.dep) {
        summary.note_state_dependency(dependency.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::QuirksMode;
    use crate::invalidation::element::invalidation_map::note_selector_for_invalidation;
    use crate::selector_parser::SelectorParser;
    use crate::stylesheets::UrlExtraData;

    fn lightmount_dependency_summary_for_selector(
        selector: &str,
    ) -> LightmountDependencyInvalidationSummary {
        let url_data = UrlExtraData::from(url::Url::parse("https://example.test/").unwrap());
        let selectors = SelectorParser::parse_author_origin_no_namespace(selector, &url_data)
            .expect("selector should parse");
        let mut map = InvalidationMap::new();
        let mut relative_map = InvalidationMap::new();
        let mut additional_relative_map = AdditionalRelativeSelectorInvalidationMap::new();

        for selector in selectors.slice() {
            note_selector_for_invalidation(
                selector,
                QuirksMode::NoQuirks,
                &mut map,
                &mut relative_map,
                &mut additional_relative_map,
                None,
                None,
            )
            .expect("selector invalidation dependencies should collect");
        }

        let mut summary = lightmount_dependency_summary_for_invalidation_map(&map);
        summary.extend(lightmount_dependency_summary_for_invalidation_map(
            &relative_map,
        ));
        summary.extend(lightmount_dependency_summary_for_relative_invalidation_map(
            &additional_relative_map,
        ));
        summary
    }

    #[test]
    fn lightmount_dependency_query_result_keeps_fallback_reasons_out_of_kinds() {
        let mut result = LightmountDependencyQueryResult::default();
        result.add_fallback_reason(LightmountDependencyFallbackReason::FullSelector);

        assert!(result.has_any_dependency());
        assert!(result.requires_fallback());
        assert_eq!(
            result.fallback_reasons(),
            &[LightmountDependencyFallbackReason::FullSelector]
        );
        assert_eq!(
            result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::SourceFallback
        );
        assert!(result.kinds().is_empty());
    }

    #[test]
    fn lightmount_dependency_query_result_dedupes_extended_fallback_reasons() {
        let mut first = LightmountDependencyQueryResult::default();
        first.add_fallback_reason(LightmountDependencyFallbackReason::UnknownDependency);
        let mut second = LightmountDependencyQueryResult::default();
        second.add_fallback_reason(LightmountDependencyFallbackReason::UnknownDependency);
        second.add_kind(LightmountDependencyKind::Siblings);

        first.extend(second);

        assert!(first.has_any_dependency());
        assert!(first.requires_fallback());
        assert_eq!(
            first.fallback_reasons(),
            &[LightmountDependencyFallbackReason::UnknownDependency]
        );
        assert_eq!(first.kinds(), &[LightmountDependencyKind::Siblings]);
        assert!(first.has_sibling_dependency());
    }

    #[test]
    fn lightmount_dependency_query_result_derives_shape_fallback_reasons() {
        let mut scope = LightmountDependencyQueryResult::default();
        scope.add_kind(LightmountDependencyKind::Scope);
        assert_eq!(
            scope.fallback_or_shape_reasons(),
            vec![LightmountDependencyFallbackReason::ScopeDependency]
        );

        let mut sibling = LightmountDependencyQueryResult::default();
        sibling.add_kind(LightmountDependencyKind::Siblings);
        assert_eq!(
            sibling.fallback_or_shape_reasons(),
            vec![LightmountDependencyFallbackReason::UnsupportedDependency]
        );
    }

    #[test]
    fn lightmount_dependency_query_result_exposes_relative_shape_predicates() {
        let mut direct_previous = LightmountDependencyQueryResult::default();
        direct_previous.add_kind(LightmountDependencyKind::RelativePrevSibling);
        direct_previous.add_kind(LightmountDependencyKind::Siblings);
        assert!(direct_previous.has_relative_selector_dependency());
        assert!(direct_previous.has_relative_previous_sibling_dependency());
        assert!(direct_previous.has_only_direct_relative_previous_sibling_dependency());

        let mut ancestor_previous = LightmountDependencyQueryResult::default();
        ancestor_previous.add_kind(LightmountDependencyKind::RelativeAncestorPrevSibling);
        assert!(ancestor_previous.has_relative_selector_dependency());
        assert!(ancestor_previous.has_relative_previous_sibling_dependency());
        assert!(!ancestor_previous.has_only_direct_relative_previous_sibling_dependency());

        let mut ancestor = LightmountDependencyQueryResult::default();
        ancestor.add_kind(LightmountDependencyKind::RelativeAncestors);
        assert!(ancestor.has_relative_selector_dependency());
        assert!(!ancestor.has_relative_previous_sibling_dependency());
        assert!(!ancestor.has_only_direct_relative_previous_sibling_dependency());
    }

    #[test]
    fn lightmount_dependency_query_result_exposes_context_root_flags() {
        let mut query = LightmountDependencyQueryResult::default();
        query.add_kind(LightmountDependencyKind::ElementAndDescendants);
        query.add_kind(LightmountDependencyKind::Siblings);
        query.add_kind(LightmountDependencyKind::SlottedElements);
        query.add_kind(LightmountDependencyKind::Parts);
        query.add_kind(LightmountDependencyKind::RelativePrevSibling);
        query.add_kind(LightmountDependencyKind::RelativeAncestorEarlierSibling);
        let flags = query.context_root_flags();

        assert!(flags.local_subtree);
        assert!(flags.following_siblings);
        assert!(flags.slotted_elements);
        assert!(flags.parts);
        assert!(flags.direct_previous_sibling_relative);
        assert!(flags.previous_sibling);
        assert!(flags.ancestor_chain);
        assert!(flags.ancestor_earlier_siblings);
        assert!(!flags.earlier_siblings);
        assert!(!flags.ancestor_previous_siblings);
        assert!(!flags.requires_source_fallback);

        query.add_kind(LightmountDependencyKind::Scope);
        assert!(query.context_root_flags().requires_source_fallback);
    }

    #[test]
    fn lightmount_relative_summary_does_not_treat_used_flag_as_unknown_dependency() {
        let mut map = AdditionalRelativeSelectorInvalidationMap::new();
        map.used = true;

        let summary = lightmount_dependency_summary_for_relative_invalidation_map(&map);

        assert!(!summary.has_unknown_dependency());
        assert!(!summary.query_universal().has_any_dependency());
    }

    #[test]
    fn lightmount_relative_summary_keeps_ancestor_traversal_unknown() {
        let mut map = AdditionalRelativeSelectorInvalidationMap::new();
        map.needs_ancestors_traversal = true;

        let summary = lightmount_dependency_summary_for_relative_invalidation_map(&map);

        assert!(summary.has_unknown_dependency());
    }

    #[test]
    fn lightmount_nth_of_dependencies_are_sibling_sensitive_by_key() {
        let mut summary = LightmountDependencyInvalidationSummary::default();
        let class = Atom::from("c");
        let other_class = Atom::from("other");
        let id = Atom::from("target");
        let attribute = LocalName::from("data-active");

        summary.note_nth_of_class_dependency(class.clone());
        summary.note_nth_of_id_dependency(id.clone());
        summary.note_nth_of_attribute_dependency(attribute.clone());
        summary.note_nth_of_state_dependency(ElementState::FOCUS);
        summary.note_nth_of_state_dependency(ElementState::empty());

        assert!(!summary.has_unknown_dependency());
        let class_result = summary.query_class(&class);
        assert!(class_result.requires_fallback());
        assert_eq!(class_result.kinds(), &[LightmountDependencyKind::Siblings]);
        assert_eq!(
            class_result.fallback_reasons(),
            &[LightmountDependencyFallbackReason::NthOfDependency]
        );
        assert_eq!(
            class_result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::ContextRoots
        );
        assert!(!summary.query_class(&other_class).has_any_dependency());
        assert_eq!(
            summary.query_id(&id).kinds(),
            &[LightmountDependencyKind::Siblings]
        );
        assert_eq!(
            summary.query_attribute(&attribute).kinds(),
            &[LightmountDependencyKind::Siblings]
        );
        assert_eq!(
            summary.query_focus().kinds(),
            &[LightmountDependencyKind::Siblings]
        );
    }

    #[test]
    fn lightmount_summary_marks_nested_relative_selector_lists_for_fallback() {
        let summary = lightmount_dependency_summary_for_selector(
            "#target:has(:is(.item + .item + .item > .child + .child + .child))",
        );

        let item_result = summary.query_class(&Atom::from("item"));
        assert!(item_result.has_any_dependency());
        assert!(item_result.requires_fallback());
        assert!(item_result
            .fallback_reasons()
            .contains(&LightmountDependencyFallbackReason::NestedRelativeSelectorDependency));
        assert_eq!(
            item_result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::ContextRoots
        );

        let child_result = summary.query_class(&Atom::from("child"));
        assert!(child_result.has_any_dependency());
        assert!(child_result.requires_fallback());
        assert!(child_result
            .fallback_reasons()
            .contains(&LightmountDependencyFallbackReason::NestedRelativeSelectorDependency));
        assert_eq!(
            child_result.fallback_root_policy(),
            LightmountDependencyFallbackRootPolicy::ContextRoots
        );
    }

    #[test]
    fn lightmount_summary_exposes_link_pseudos_as_href_attribute_dependencies() {
        let summary = lightmount_dependency_summary_for_selector("#target:has(:any-link)");
        let href = LocalName::from("href");

        assert!(summary.query_attribute(&href).has_any_dependency());
        assert!(!summary
            .query_attribute(&LocalName::from("class"))
            .has_any_dependency());
    }
}

fn lightmount_selector_map_has_sibling_dependency<T: 'static>(
    map: &SelectorMap<T>,
    dependency: impl Fn(&T) -> &Dependency + Copy,
) -> bool {
    map.root
        .iter()
        .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        || map.id_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.class_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.attribute_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.local_name_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map.namespace_hash.iter().any(|(_, entries)| {
            entries
                .iter()
                .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        })
        || map
            .rare_pseudo_classes
            .iter()
            .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
        || map
            .other
            .iter()
            .any(|entry| lightmount_dependency_is_sibling_sensitive(dependency(entry)))
}

fn lightmount_dependency_is_sibling_sensitive(dependency: &Dependency) -> bool {
    match dependency.invalidation_kind() {
        DependencyInvalidationKind::Normal(NormalDependencyInvalidationKind::Siblings) => true,
        DependencyInvalidationKind::Relative(
            RelativeDependencyInvalidationKind::PrevSibling
            | RelativeDependencyInvalidationKind::AncestorPrevSibling
            | RelativeDependencyInvalidationKind::EarlierSibling
            | RelativeDependencyInvalidationKind::AncestorEarlierSibling,
        ) => true,
        _ => {
            dependency.right_combinator_is_next_sibling()
                || dependency.dependency_is_relative_with_single_next_sibling()
        },
    }
}
