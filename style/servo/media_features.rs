/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Servo's media feature list and evaluator.

use crate::derives::*;
use crate::media_queries::MediaType;
use crate::queries::feature::{AllowsRanges, Evaluator, FeatureFlags, QueryFeatureDescription};
use crate::queries::values::{Orientation, PrefersColorScheme};
use crate::values::computed::{CSSPixelLength, Context, Ratio, Resolution};
use crate::values::specified::color::ForcedColors;
use std::fmt::Debug;

/// https://drafts.csswg.org/mediaqueries-4/#width
fn eval_width(context: &Context) -> CSSPixelLength {
    CSSPixelLength::new(context.device().au_viewport_size().width.to_f32_px())
}

/// https://drafts.csswg.org/mediaqueries-4/#device-width
fn eval_device_width(context: &Context) -> CSSPixelLength {
    // Servo's generic Device has no separate screen-size concept. Use the
    // viewport as the closest available device-size signal.
    eval_width(context)
}

/// https://drafts.csswg.org/mediaqueries-4/#height
fn eval_height(context: &Context) -> CSSPixelLength {
    CSSPixelLength::new(context.device().au_viewport_size().height.to_f32_px())
}

/// https://drafts.csswg.org/mediaqueries-4/#device-height
fn eval_device_height(context: &Context) -> CSSPixelLength {
    eval_height(context)
}

/// https://drafts.csswg.org/mediaqueries-4/#aspect-ratio
fn eval_aspect_ratio(context: &Context) -> Ratio {
    let size = context.device().au_viewport_size();
    Ratio::new(size.width.0 as f32, size.height.0 as f32)
}

/// https://drafts.csswg.org/mediaqueries-4/#device-aspect-ratio
fn eval_device_aspect_ratio(context: &Context) -> Ratio {
    eval_aspect_ratio(context)
}

/// https://drafts.csswg.org/mediaqueries-4/#orientation
fn eval_orientation(context: &Context, value: Option<Orientation>) -> bool {
    Orientation::eval(context.device().au_viewport_size(), value)
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum Scan {
    Progressive,
    Interlace,
}

/// https://drafts.csswg.org/mediaqueries-4/#scan
fn eval_scan(_: &Context, _: Option<Scan>) -> bool {
    // Since we doesn't support the 'tv' media type, the 'scan' feature never
    // matches.
    false
}

/// Values for the display-mode media feature.
#[derive(Clone, Copy, Debug, FromPrimitive, Parse, PartialEq, ToCss)]
#[repr(u8)]
#[allow(missing_docs)]
pub enum DisplayMode {
    Browser,
    MinimalUi,
    Standalone,
    Fullscreen,
}

/// https://w3c.github.io/manifest/#the-display-mode-media-feature
fn eval_display_mode(_: &Context, query_value: Option<DisplayMode>) -> bool {
    query_value.map_or(true, |value| value == DisplayMode::Browser)
}

/// https://drafts.csswg.org/mediaqueries-4/#grid
fn eval_grid(_: &Context) -> bool {
    false
}

/// https://drafts.csswg.org/mediaqueries-4/#color
fn eval_color(_: &Context) -> i32 {
    24
}

/// https://drafts.csswg.org/mediaqueries-4/#color-index
fn eval_color_index(_: &Context) -> i32 {
    0
}

/// https://drafts.csswg.org/mediaqueries-4/#monochrome
fn eval_monochrome(_: &Context) -> i32 {
    0
}

/// Values for the color-gamut media feature.
#[derive(Clone, Copy, Debug, FromPrimitive, Parse, PartialEq, PartialOrd, ToCss)]
#[repr(u8)]
pub enum ColorGamut {
    /// The sRGB gamut.
    Srgb,
    /// The gamut specified by the Display P3 Color Space.
    P3,
    /// The gamut specified by the ITU-R Recommendation BT.2020 Color Space.
    Rec2020,
}

/// https://drafts.csswg.org/mediaqueries-4/#color-gamut
fn eval_color_gamut(_: &Context, query_value: Option<ColorGamut>) -> bool {
    query_value.is_some_and(|value| value <= ColorGamut::Srgb)
}

/// https://drafts.csswg.org/mediaqueries-4/#resolution
fn eval_resolution(context: &Context) -> Resolution {
    Resolution::from_dppx(context.device().device_pixel_ratio().0)
}

/// https://compat.spec.whatwg.org/#css-media-queries-webkit-device-pixel-ratio
fn eval_device_pixel_ratio(context: &Context) -> f32 {
    eval_resolution(context).dppx()
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum PrefersReducedMotion {
    NoPreference,
    Reduce,
}

/// https://drafts.csswg.org/mediaqueries-5/#prefers-reduced-motion
fn eval_prefers_reduced_motion(
    context: &Context,
    query_value: Option<PrefersReducedMotion>,
) -> bool {
    let prefers_reduced = context
        .device()
        .media_feature_preferences()
        .prefers_reduced_motion;
    match query_value {
        None => prefers_reduced,
        Some(PrefersReducedMotion::NoPreference) => !prefers_reduced,
        Some(PrefersReducedMotion::Reduce) => prefers_reduced,
    }
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum PrefersReducedData {
    NoPreference,
    Reduce,
}

/// https://drafts.csswg.org/mediaqueries-5/#prefers-reduced-data
fn eval_prefers_reduced_data(context: &Context, query_value: Option<PrefersReducedData>) -> bool {
    let prefers_reduced = context
        .device()
        .media_feature_preferences()
        .prefers_reduced_data;
    match query_value {
        None => prefers_reduced,
        Some(PrefersReducedData::NoPreference) => !prefers_reduced,
        Some(PrefersReducedData::Reduce) => prefers_reduced,
    }
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum PrefersReducedTransparency {
    NoPreference,
    Reduce,
}

/// https://drafts.csswg.org/mediaqueries-5/#prefers-reduced-transparency
fn eval_prefers_reduced_transparency(
    context: &Context,
    query_value: Option<PrefersReducedTransparency>,
) -> bool {
    let prefers_reduced = context
        .device()
        .media_feature_preferences()
        .prefers_reduced_transparency;
    match query_value {
        None => prefers_reduced,
        Some(PrefersReducedTransparency::NoPreference) => !prefers_reduced,
        Some(PrefersReducedTransparency::Reduce) => prefers_reduced,
    }
}

/// Possible values for prefers-contrast media query.
#[derive(Clone, Copy, Debug, FromPrimitive, Parse, PartialEq, ToCss)]
#[repr(u8)]
pub enum PrefersContrast {
    /// More contrast is preferred.
    More,
    /// Low contrast is preferred.
    Less,
    /// Custom contrast is preferred.
    Custom,
    /// The default value if neither high or low contrast is enabled.
    NoPreference,
}

/// https://drafts.csswg.org/mediaqueries-5/#prefers-contrast
fn eval_prefers_contrast(context: &Context, query_value: Option<PrefersContrast>) -> bool {
    let prefers_contrast = context
        .device()
        .media_feature_preferences()
        .prefers_contrast;
    match query_value {
        Some(value) => value == prefers_contrast,
        None => prefers_contrast != PrefersContrast::NoPreference,
    }
}

/// https://drafts.csswg.org/mediaqueries-5/#forced-colors
fn eval_forced_colors(context: &Context, query_value: Option<ForcedColors>) -> bool {
    let forced = context.device().forced_colors();
    match query_value {
        Some(query_value) => query_value == forced,
        None => forced != ForcedColors::None,
    }
}

fn eval_prefers_color_scheme(context: &Context, query_value: Option<PrefersColorScheme>) -> bool {
    match query_value {
        Some(v) => context.device().color_scheme() == v,
        None => true,
    }
}

/// Values for the dynamic-range and video-dynamic-range media features.
#[derive(Clone, Copy, Debug, FromPrimitive, Parse, PartialEq, PartialOrd, ToCss)]
#[repr(u8)]
#[allow(missing_docs)]
pub enum DynamicRange {
    Standard,
    High,
}

/// https://drafts.csswg.org/mediaqueries-5/#dynamic-range
fn eval_dynamic_range(_: &Context, query_value: Option<DynamicRange>) -> bool {
    query_value.is_some_and(|value| value <= DynamicRange::Standard)
}

/// https://drafts.csswg.org/mediaqueries-5/#video-dynamic-range
fn eval_video_dynamic_range(_: &Context, query_value: Option<DynamicRange>) -> bool {
    query_value.is_some_and(|value| value <= DynamicRange::Standard)
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum Update {
    None,
    Slow,
    Fast,
}

/// https://drafts.csswg.org/mediaqueries-4/#update
fn eval_update(context: &Context, query_value: Option<Update>) -> bool {
    let can_update = context.device().media_type() != MediaType::print();
    let query_value = match query_value {
        Some(value) => value,
        None => return can_update,
    };
    match query_value {
        Update::None => !can_update,
        Update::Slow => false,
        Update::Fast => can_update,
    }
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum Scripting {
    None,
    InitialOnly,
    Enabled,
}

/// https://drafts.csswg.org/mediaqueries-5/#scripting
fn eval_scripting(_: &Context, query_value: Option<Scripting>) -> bool {
    query_value.map_or(true, |value| matches!(value, Scripting::Enabled))
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum Pointer {
    None,
    Coarse,
    Fine,
}

fn eval_pointer_capabilities(query_value: Option<Pointer>) -> bool {
    match query_value {
        Some(Pointer::None) => false,
        Some(Pointer::Coarse) => false,
        Some(Pointer::Fine) => true,
        None => true,
    }
}

/// https://drafts.csswg.org/mediaqueries-4/#pointer
fn eval_pointer(_: &Context, query_value: Option<Pointer>) -> bool {
    eval_pointer_capabilities(query_value)
}

/// https://drafts.csswg.org/mediaqueries-4/#descdef-media-any-pointer
fn eval_any_pointer(_: &Context, query_value: Option<Pointer>) -> bool {
    eval_pointer_capabilities(query_value)
}

#[derive(Clone, Copy, Debug, FromPrimitive, Parse, ToCss)]
#[repr(u8)]
enum Hover {
    None,
    Hover,
}

fn eval_hover_capabilities(query_value: Option<Hover>) -> bool {
    match query_value {
        Some(Hover::None) => false,
        Some(Hover::Hover) => true,
        None => true,
    }
}

/// https://drafts.csswg.org/mediaqueries-4/#hover
fn eval_hover(_: &Context, query_value: Option<Hover>) -> bool {
    eval_hover_capabilities(query_value)
}

/// https://drafts.csswg.org/mediaqueries-4/#descdef-media-any-hover
fn eval_any_hover(_: &Context, query_value: Option<Hover>) -> bool {
    eval_hover_capabilities(query_value)
}

/// A list with all the media features that Servo supports.
pub static MEDIA_FEATURES: [QueryFeatureDescription; 31] = [
    feature!(
        atom!("width"),
        AllowsRanges::Yes,
        Evaluator::Length(eval_width),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("height"),
        AllowsRanges::Yes,
        Evaluator::Length(eval_height),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("aspect-ratio"),
        AllowsRanges::Yes,
        Evaluator::NumberRatio(eval_aspect_ratio),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("orientation"),
        AllowsRanges::No,
        keyword_evaluator!(eval_orientation, Orientation),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("device-width"),
        AllowsRanges::Yes,
        Evaluator::Length(eval_device_width),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("device-height"),
        AllowsRanges::Yes,
        Evaluator::Length(eval_device_height),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("device-aspect-ratio"),
        AllowsRanges::Yes,
        Evaluator::NumberRatio(eval_device_aspect_ratio),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("display-mode"),
        AllowsRanges::No,
        keyword_evaluator!(eval_display_mode, DisplayMode),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("grid"),
        AllowsRanges::No,
        Evaluator::BoolInteger(eval_grid),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("scan"),
        AllowsRanges::No,
        keyword_evaluator!(eval_scan, Scan),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("color"),
        AllowsRanges::Yes,
        Evaluator::Integer(eval_color),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("color-index"),
        AllowsRanges::Yes,
        Evaluator::Integer(eval_color_index),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("monochrome"),
        AllowsRanges::Yes,
        Evaluator::Integer(eval_monochrome),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("color-gamut"),
        AllowsRanges::No,
        keyword_evaluator!(eval_color_gamut, ColorGamut),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("resolution"),
        AllowsRanges::Yes,
        Evaluator::Resolution(eval_resolution),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("device-pixel-ratio"),
        AllowsRanges::Yes,
        Evaluator::Float(eval_device_pixel_ratio),
        FeatureFlags::WEBKIT_PREFIX,
    ),
    feature!(
        atom!("-moz-device-pixel-ratio"),
        AllowsRanges::Yes,
        Evaluator::Float(eval_device_pixel_ratio),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("prefers-reduced-motion"),
        AllowsRanges::No,
        keyword_evaluator!(eval_prefers_reduced_motion, PrefersReducedMotion),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("prefers-reduced-data"),
        AllowsRanges::No,
        keyword_evaluator!(eval_prefers_reduced_data, PrefersReducedData),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("prefers-reduced-transparency"),
        AllowsRanges::No,
        keyword_evaluator!(
            eval_prefers_reduced_transparency,
            PrefersReducedTransparency
        ),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("prefers-contrast"),
        AllowsRanges::No,
        keyword_evaluator!(eval_prefers_contrast, PrefersContrast),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("forced-colors"),
        AllowsRanges::No,
        keyword_evaluator!(eval_forced_colors, ForcedColors),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("prefers-color-scheme"),
        AllowsRanges::No,
        keyword_evaluator!(eval_prefers_color_scheme, PrefersColorScheme),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("dynamic-range"),
        AllowsRanges::No,
        keyword_evaluator!(eval_dynamic_range, DynamicRange),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("video-dynamic-range"),
        AllowsRanges::No,
        keyword_evaluator!(eval_video_dynamic_range, DynamicRange),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("update"),
        AllowsRanges::No,
        keyword_evaluator!(eval_update, Update),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("scripting"),
        AllowsRanges::No,
        keyword_evaluator!(eval_scripting, Scripting),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("pointer"),
        AllowsRanges::No,
        keyword_evaluator!(eval_pointer, Pointer),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("any-pointer"),
        AllowsRanges::No,
        keyword_evaluator!(eval_any_pointer, Pointer),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("hover"),
        AllowsRanges::No,
        keyword_evaluator!(eval_hover, Hover),
        FeatureFlags::empty(),
    ),
    feature!(
        atom!("any-hover"),
        AllowsRanges::No,
        keyword_evaluator!(eval_any_hover, Hover),
        FeatureFlags::empty(),
    ),
];
