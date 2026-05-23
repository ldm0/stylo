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
    /// Full-selector invalidation is required.
    FullSelector,
}

/// Conservative query result for one changed class/id/attribute/state token.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountDependencyQueryResult {
    kinds: Vec<LightmountDependencyKind>,
    unknown_dependency: bool,
}

impl LightmountDependencyQueryResult {
    fn add_kind(&mut self, kind: LightmountDependencyKind) {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
    }

    fn extend(&mut self, other: Self) {
        for kind in other.kinds {
            self.add_kind(kind);
        }
        self.unknown_dependency |= other.unknown_dependency;
    }

    /// Returns whether any dependency matched the query.
    #[inline]
    pub fn has_any_dependency(&self) -> bool {
        self.unknown_dependency || !self.kinds.is_empty()
    }

    /// Returns the concrete dependency kinds captured for this query.
    #[inline]
    pub fn kinds(&self) -> &[LightmountDependencyKind] {
        &self.kinds
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

    /// Returns whether this query can affect `::slotted(...)` invalidation.
    #[inline]
    pub fn has_slotted_dependency(&self) -> bool {
        self.unknown_dependency
            || self
                .kinds
                .iter()
                .any(|kind| matches!(kind, LightmountDependencyKind::SlottedElements))
    }
}

/// Keyed dependency query summary exposed for Lightmount.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct LightmountDependencyInvalidationSummary {
    class_dependencies: Vec<(Atom, LightmountDependencyQueryResult)>,
    id_dependencies: Vec<(Atom, LightmountDependencyQueryResult)>,
    type_dependencies: Vec<(LocalName, LightmountDependencyQueryResult)>,
    attribute_dependencies: Vec<(LocalName, LightmountDependencyQueryResult)>,
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
            result.unknown_dependency = true;
        }
        result
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
                .chain(self.state_dependencies.iter().map(|(_, result)| result))
                .chain(std::iter::once(&self.focus_dependency))
                .chain(std::iter::once(&self.focus_within_dependency))
                .chain(std::iter::once(&self.target_dependency))
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
                .chain(self.state_dependencies.iter().map(|(_, result)| result))
                .chain(std::iter::once(&self.focus_dependency))
                .chain(std::iter::once(&self.focus_within_dependency))
                .chain(std::iter::once(&self.target_dependency))
                .any(LightmountDependencyQueryResult::has_slotted_dependency)
    }
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
    for (_, dependencies) in map.custom_state_affecting_selectors.iter() {
        if lightmount_dependency_query_result_for_dependencies(dependencies).has_any_dependency() {
            summary.mark_unknown_dependency();
        }
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
    if map.used || map.needs_ancestors_traversal || !map.any_to_selector.is_empty() {
        summary.mark_unknown_dependency();
    }
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
            result.add_kind(LightmountDependencyKind::FullSelector);
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
    if let Some(next) = dependency.next.as_ref() {
        for dependency in next.slice() {
            lightmount_collect_dependency_query_result(dependency, result);
        }
    }
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
