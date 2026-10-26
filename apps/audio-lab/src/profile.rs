//! Versioned route profile. Parsing, catalog matching, and file publication stay
//! outside audio workers. Friendly names are display hints. Endpoint ids are exact.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value};

const SCHEMA: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    RenderLoopback,
    CaptureEndpoint,
}

impl SourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RenderLoopback => "render_loopback",
            Self::CaptureEndpoint => "capture_endpoint",
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "render_loopback" => Ok(Self::RenderLoopback),
            "capture_endpoint" => Ok(Self::CaptureEndpoint),
            _ => Err("source kind must be render_loopback or capture_endpoint".into()),
        }
    }
}

impl<'de> Deserialize<'de> for SourceKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackMode {
    Remote,
    Local,
    Mix,
    Mute,
}

impl PlaybackMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
            Self::Mix => "mix",
            Self::Mute => "mute",
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "remote" => Ok(Self::Remote),
            "local" => Ok(Self::Local),
            "mix" => Ok(Self::Mix),
            "mute" => Ok(Self::Mute),
            _ => Err("play must be remote, local, mix or mute".into()),
        }
    }

    /// Original local monitoring follows local and mix playback.
    pub const fn original_monitoring(self) -> bool {
        matches!(self, Self::Local | Self::Mix)
    }
}

impl<'de> Deserialize<'de> for PlaybackMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SavedPlayback {
    pub mode: PlaybackMode,
    pub local_gain: f32,
    pub remote_gain: f32,
    pub buffer_periods: u8,
}

impl SavedPlayback {
    pub const fn remote_defaults() -> Self {
        Self {
            mode: PlaybackMode::Remote,
            local_gain: 1.0,
            remote_gain: 1.0,
            buffer_periods: 2,
        }
    }
}

impl Default for SavedPlayback {
    fn default() -> Self {
        Self::remote_defaults()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouteProfile {
    source_id: String,
    source_kind: SourceKind,
    source_label: Option<String>,
    output_id: String,
    output_label: Option<String>,
    playback: PlaybackMode,
    local_gain: f32,
    remote_gain: f32,
    buffer_periods: u8,
}

impl RouteProfile {
    pub fn from_selection(
        source_id: &str,
        source_kind: SourceKind,
        source_label: Option<&str>,
        output_id: &str,
        output_label: Option<&str>,
        playback: SavedPlayback,
    ) -> Result<Self, String> {
        validated(
            source_id.to_owned(),
            source_kind,
            source_label.map(str::to_owned),
            output_id.to_owned(),
            output_label.map(str::to_owned),
            playback,
        )
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn source_kind(&self) -> SourceKind {
        self.source_kind
    }

    pub fn output_id(&self) -> &str {
        &self.output_id
    }

    pub fn playback(&self) -> SavedPlayback {
        SavedPlayback {
            mode: self.playback,
            local_gain: self.local_gain,
            remote_gain: self.remote_gain,
            buffer_periods: self.buffer_periods,
        }
    }

    pub fn saved_configuration(&self) -> String {
        format!(
            "schema={SCHEMA}\nsource_id={}\nsource_kind={}\nsource_label={}\noutput_id={}\noutput_label={}\nplayback={}\nlocal_gain={}\nremote_gain={}\nbuffer_periods={}\noriginal_monitoring={}\nlabels=display_hints",
            self.source_id,
            self.source_kind.as_str(),
            show_label(self.source_label.as_deref()),
            self.output_id,
            show_label(self.output_label.as_deref()),
            self.playback.as_str(),
            self.local_gain,
            self.remote_gain,
            self.buffer_periods,
            if self.playback.original_monitoring() {
                "on"
            } else {
                "off"
            },
        )
    }

    pub fn inspection_report(&self) -> String {
        format!(
            "{}\ninspection=profile_file_only\ndevice_enumeration=not_used\nstream_open=not_used",
            self.saved_configuration()
        )
    }

    fn catalog_assessment(&self, catalog: &RouteCatalog) -> (PinReport, PinReport, RouteStatus) {
        let source_need = match self.source_kind {
            SourceKind::RenderLoopback => DirectionNeed::Render,
            SourceKind::CaptureEndpoint => DirectionNeed::Capture,
        };
        let source = resolve_endpoint(
            "source",
            &self.source_id,
            source_need,
            self.source_kind.as_str(),
            catalog,
        );
        let output = resolve_endpoint(
            "output",
            &self.output_id,
            DirectionNeed::Render,
            "output",
            catalog,
        );
        let same = self.source_id == self.output_id;
        let route_status = if matches!(
            (source.status, output.status),
            (RouteStatus::Unavailable, _) | (_, RouteStatus::Unavailable)
        ) {
            RouteStatus::Unavailable
        } else if same
            || source.status != RouteStatus::Eligible
            || output.status != RouteStatus::Eligible
        {
            RouteStatus::Unsupported
        } else {
            RouteStatus::Eligible
        };
        (source, output, route_status)
    }

    /// Shared gate for profile execution. A catalog result cannot guarantee
    /// later stream negotiation, but ineligible selections must never open.
    pub fn require_catalog_eligible(&self, catalog: &RouteCatalog) -> Result<(), String> {
        if self.catalog_assessment(catalog).2 == RouteStatus::Eligible {
            Ok(())
        } else {
            Err(format!(
                "saved route cannot open:\n{}",
                self.preflight_report(catalog)
            ))
        }
    }

    pub fn preflight_report(&self, catalog: &RouteCatalog) -> String {
        let (source, output, route_status) = self.catalog_assessment(catalog);
        let same = self.source_id == self.output_id;
        let separation = if same { "same_endpoint" } else { "distinct" };
        let separation_reason = if same {
            "source and output pin the same endpoint id"
        } else {
            "source and output pin different endpoint ids"
        };
        format!(
            "{}\n{}\n{}\nseparation={separation}\nseparation_reason={separation_reason}\nroute_status={}\nnegotiation=not_attempted\nnegotiation_scope=catalog eligibility leaves stream open and period negotiation to a later command",
            self.saved_configuration(),
            source.lines("source", &self.source_id),
            output.lines("output", &self.output_id),
            route_status.as_str(),
        )
    }

    fn to_json_bytes(&self) -> Result<Vec<u8>, String> {
        let value = Value::Object(object([
            ("schema", Value::from(SCHEMA)),
            (
                "source",
                Value::Object(source_object(
                    &self.source_id,
                    self.source_kind,
                    self.source_label.as_deref(),
                )),
            ),
            (
                "output",
                Value::Object(output_object(&self.output_id, self.output_label.as_deref())),
            ),
            ("playback", Value::from(self.playback.as_str())),
            ("local_gain", json_gain(self.local_gain)?),
            ("remote_gain", json_gain(self.remote_gain)?),
            ("buffer_periods", Value::from(self.buffer_periods)),
        ]));
        let mut bytes = serde_json::to_vec_pretty(&value)
            .map_err(|error| format!("unable to encode the route profile: {error}"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

pub fn parse_profile(text: &str) -> Result<RouteProfile, String> {
    let document: RouteDocument =
        serde_json::from_str(text).map_err(|error| format!("invalid route profile: {error}"))?;
    if document.schema != SCHEMA {
        return Err(format!(
            "schema version {} is unsupported; this lab reads schema {SCHEMA}",
            document.schema
        ));
    }
    validated(
        document.source.endpoint_id,
        document.source.kind,
        document.source.label,
        document.output.endpoint_id,
        document.output.label,
        SavedPlayback {
            mode: document.playback,
            local_gain: document.local_gain,
            remote_gain: document.remote_gain,
            buffer_periods: u8::try_from(document.buffer_periods).unwrap_or(0),
        },
    )
}

pub fn load_profile(path: &Path) -> Result<RouteProfile, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("unable to read {}: {error}", path.display()))?;
    parse_profile(&text)
}

pub fn create_profile(path: &Path, profile: &RouteProfile) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "profile {} already exists; the existing file was not modified",
            path.display()
        ));
    }
    let bytes = profile.to_json_bytes()?;
    let staged = stage_bytes(path, &bytes)?;
    if let Err(error) = publish_staged(&staged, path, false) {
        let _ = fs::remove_file(&staged);
        if path.exists() {
            return Err(format!(
                "profile {} was left unchanged: {error}",
                path.display()
            ));
        }
        return Err(format!("unable to create {}: {error}", path.display()));
    }
    Ok(())
}

/// Replaces a whole existing profile after a complete staging write.
/// A failed staging or publish leaves the previous file bytes in place.
#[cfg_attr(not(test), allow(dead_code))]
pub fn replace_profile(path: &Path, profile: &RouteProfile) -> Result<(), String> {
    let bytes = profile.to_json_bytes()?;
    if !path.is_file() {
        return Err(format!(
            "profile {} is not an existing file; nothing was written",
            path.display()
        ));
    }
    let previous =
        fs::read(path).map_err(|error| format!("unable to read {}: {error}", path.display()))?;
    let staged = stage_bytes(path, &bytes)?;
    if let Err(error) = publish_staged(&staged, path, true) {
        let _ = fs::remove_file(&staged);
        let current = fs::read(path).ok();
        if current.as_deref() == Some(previous.as_slice()) {
            return Err(format!(
                "profile {} was left unchanged: {error}",
                path.display()
            ));
        }
        return Err(format!(
            "profile update failed for {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

pub fn validate_gain(name: &str, gain: f32) -> Result<(), String> {
    if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
        return Err(format!("{name} must be finite in 0..1"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogDirection {
    Render,
    Capture,
    Other,
}

impl CatalogDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Capture => "capture",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogState {
    Active,
    Disabled,
    Unplugged,
    NotPresent,
}

impl CatalogState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Unplugged => "unplugged",
            Self::NotPresent => "not_present",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub valid_bits_per_sample: u16,
    pub block_align: u16,
    pub channel_mask: u32,
    pub float32: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEndpoint {
    pub id: String,
    pub label: String,
    pub direction: CatalogDirection,
    pub state: CatalogState,
    pub format: Option<EndpointFormat>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteCatalog {
    endpoints: Vec<CatalogEndpoint>,
}

impl RouteCatalog {
    pub fn from_endpoints(endpoints: Vec<CatalogEndpoint>) -> Self {
        Self { endpoints }
    }
}

pub fn is_native_lab_format(format: &EndpointFormat) -> bool {
    native_sample_body(format) && matches!(format.channel_mask, 0 | 3)
}

fn validated(
    source_id: String,
    source_kind: SourceKind,
    source_label: Option<String>,
    output_id: String,
    output_label: Option<String>,
    playback: SavedPlayback,
) -> Result<RouteProfile, String> {
    validate_endpoint_id("source", &source_id)?;
    validate_endpoint_id("output", &output_id)?;
    validate_gain("local_gain", playback.local_gain)?;
    validate_gain("remote_gain", playback.remote_gain)?;
    if !(1..=8).contains(&playback.buffer_periods) {
        return Err("buffer periods must be 1..8".into());
    }
    Ok(RouteProfile {
        source_id,
        source_kind,
        source_label,
        output_id,
        output_label,
        playback: playback.mode,
        local_gain: playback.local_gain,
        remote_gain: playback.remote_gain,
        buffer_periods: playback.buffer_periods,
    })
}

fn validate_endpoint_id(role: &str, id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err(format!("{role} endpoint id is empty"));
    }
    if id != id.trim() {
        return Err(format!(
            "{role} endpoint id must keep its exact opaque characters, including the edges"
        ));
    }
    Ok(())
}

fn show_label(label: Option<&str>) -> String {
    match label {
        Some(label) if !label.is_empty() => label.replace(['\n', '\r'], " "),
        _ => "-".into(),
    }
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_owned(), value);
    }
    map
}

fn source_object(id: &str, kind: SourceKind, label: Option<&str>) -> Map<String, Value> {
    let mut map = object([
        ("endpoint_id", Value::from(id)),
        ("kind", Value::from(kind.as_str())),
    ]);
    if let Some(label) = label {
        map.insert("label".to_owned(), Value::from(label));
    }
    map
}

fn output_object(id: &str, label: Option<&str>) -> Map<String, Value> {
    let mut map = object([("endpoint_id", Value::from(id))]);
    if let Some(label) = label {
        map.insert("label".to_owned(), Value::from(label));
    }
    map
}

fn json_gain(gain: f32) -> Result<Value, String> {
    serde_json::to_value(gain).map_err(|error| format!("unable to encode gain: {error}"))
}

#[derive(Debug)]
struct RouteDocument {
    schema: u64,
    source: SourceDocument,
    output: OutputDocument,
    playback: PlaybackMode,
    local_gain: f32,
    remote_gain: f32,
    buffer_periods: u64,
}

#[derive(Debug)]
struct SourceDocument {
    endpoint_id: String,
    kind: SourceKind,
    label: Option<String>,
}

#[derive(Debug)]
struct OutputDocument {
    endpoint_id: String,
    label: Option<String>,
}

impl<'de> Deserialize<'de> for RouteDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(RouteVisitor)
    }
}

struct RouteVisitor;

impl<'de> Visitor<'de> for RouteVisitor {
    type Value = RouteDocument;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("route profile object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        const FIELDS: &[&str] = &[
            "schema",
            "source",
            "output",
            "playback",
            "local_gain",
            "remote_gain",
            "buffer_periods",
        ];
        let mut seen = Vec::new();
        let mut schema = None;
        let mut source = None;
        let mut output = None;
        let mut playback = None;
        let mut local_gain = None;
        let mut remote_gain = None;
        let mut buffer_periods = None;
        while let Some(key) = map.next_key::<String>()? {
            note_field::<A::Error>(&mut seen, &key, FIELDS)?;
            match key.as_str() {
                "schema" => schema = Some(map.next_value()?),
                "source" => source = Some(map.next_value()?),
                "output" => output = Some(map.next_value()?),
                "playback" => playback = Some(map.next_value()?),
                "local_gain" => local_gain = Some(map.next_value()?),
                "remote_gain" => remote_gain = Some(map.next_value()?),
                "buffer_periods" => buffer_periods = Some(map.next_value()?),
                _ => return Err(de::Error::unknown_field(&key, FIELDS)),
            }
        }
        Ok(RouteDocument {
            schema: schema.ok_or_else(|| de::Error::missing_field("schema"))?,
            source: source.ok_or_else(|| de::Error::missing_field("source"))?,
            output: output.ok_or_else(|| de::Error::missing_field("output"))?,
            playback: playback.ok_or_else(|| de::Error::missing_field("playback"))?,
            local_gain: local_gain.ok_or_else(|| de::Error::missing_field("local_gain"))?,
            remote_gain: remote_gain.ok_or_else(|| de::Error::missing_field("remote_gain"))?,
            buffer_periods: buffer_periods
                .ok_or_else(|| de::Error::missing_field("buffer_periods"))?,
        })
    }
}

impl<'de> Deserialize<'de> for SourceDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(SourceVisitor)
    }
}

struct SourceVisitor;

impl<'de> Visitor<'de> for SourceVisitor {
    type Value = SourceDocument;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("source endpoint object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        const FIELDS: &[&str] = &["endpoint_id", "kind", "label"];
        let mut seen = Vec::new();
        let mut endpoint_id = None;
        let mut kind = None;
        let mut label = None;
        while let Some(key) = map.next_key::<String>()? {
            note_field::<A::Error>(&mut seen, &key, FIELDS)?;
            match key.as_str() {
                "endpoint_id" => endpoint_id = Some(map.next_value()?),
                "kind" => kind = Some(map.next_value()?),
                "label" => label = Some(map.next_value::<String>()?),
                _ => return Err(de::Error::unknown_field(&key, FIELDS)),
            }
        }
        Ok(SourceDocument {
            endpoint_id: endpoint_id.ok_or_else(|| de::Error::missing_field("endpoint_id"))?,
            kind: kind.ok_or_else(|| de::Error::missing_field("kind"))?,
            label,
        })
    }
}

impl<'de> Deserialize<'de> for OutputDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(OutputVisitor)
    }
}

struct OutputVisitor;

impl<'de> Visitor<'de> for OutputVisitor {
    type Value = OutputDocument;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("output endpoint object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        const FIELDS: &[&str] = &["endpoint_id", "label"];
        let mut seen = Vec::new();
        let mut endpoint_id = None;
        let mut label = None;
        while let Some(key) = map.next_key::<String>()? {
            note_field::<A::Error>(&mut seen, &key, FIELDS)?;
            match key.as_str() {
                "endpoint_id" => endpoint_id = Some(map.next_value()?),
                "label" => label = Some(map.next_value::<String>()?),
                _ => return Err(de::Error::unknown_field(&key, FIELDS)),
            }
        }
        Ok(OutputDocument {
            endpoint_id: endpoint_id.ok_or_else(|| de::Error::missing_field("endpoint_id"))?,
            label,
        })
    }
}

fn note_field<E: de::Error>(
    seen: &mut Vec<String>,
    key: &str,
    allowed: &'static [&'static str],
) -> Result<(), E> {
    if seen.iter().any(|item| item == key) {
        return Err(E::custom(format!("duplicate field `{key}`")));
    }
    if !allowed.contains(&key) {
        return Err(E::unknown_field(key, allowed));
    }
    seen.push(key.to_owned());
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteStatus {
    Eligible,
    Unavailable,
    Unsupported,
}

impl RouteStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Copy)]
enum DirectionNeed {
    Render,
    Capture,
}

impl DirectionNeed {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Capture => "capture",
        }
    }

    const fn matches(self, direction: CatalogDirection) -> bool {
        match self {
            Self::Render => matches!(direction, CatalogDirection::Render),
            Self::Capture => matches!(direction, CatalogDirection::Capture),
        }
    }
}

#[derive(Clone, Copy)]
enum FormatClass {
    Native,
    Missing,
    OutsideSubset,
    OutsideMask,
}

struct PinReport {
    status: RouteStatus,
    direction: &'static str,
    label: Option<String>,
    state: &'static str,
    format: String,
    reason: String,
}

impl PinReport {
    fn lines(&self, role: &str, pinned_id: &str) -> String {
        format!(
            "{role}_status={}\n{role}_pinned_id={pinned_id}\n{role}_current_label={}\n{role}_direction={}\n{role}_state={}\n{role}_format={}\n{role}_reason={}",
            self.status.as_str(),
            show_label(self.label.as_deref()),
            self.direction,
            self.state,
            self.format,
            self.reason,
        )
    }
}

fn resolve_endpoint(
    role: &str,
    id: &str,
    need: DirectionNeed,
    kind_label: &str,
    catalog: &RouteCatalog,
) -> PinReport {
    let matched: Vec<&CatalogEndpoint> = catalog
        .endpoints
        .iter()
        .filter(|endpoint| endpoint.id == id)
        .collect();
    let Some(endpoint) = matched.first().copied() else {
        return PinReport {
            status: RouteStatus::Unavailable,
            direction: "absent",
            label: None,
            state: "absent",
            format: "none".into(),
            reason: format!(
                "the saved {role} id is absent from the catalog and stays pinned; the saved label is not an identity"
            ),
        };
    };
    if matched.len() != 1 {
        return PinReport {
            status: RouteStatus::Unsupported,
            direction: "ambiguous",
            label: None,
            state: "ambiguous",
            format: "none".into(),
            reason: format!("the catalog lists this {role} id more than once"),
        };
    }
    let format = format_text(endpoint.format);
    if endpoint.state != CatalogState::Active {
        return PinReport {
            status: RouteStatus::Unavailable,
            direction: endpoint.direction.as_str(),
            label: Some(endpoint.label.clone()),
            state: endpoint.state.as_str(),
            format,
            reason: format!(
                "the saved {role} id is {} and stays pinned",
                endpoint.state.as_str().replace('_', " ")
            ),
        };
    }
    if !need.matches(endpoint.direction) {
        let reason = if role == "output" {
            format!(
                "the output must be a render endpoint; the catalog direction is {}",
                endpoint.direction.as_str()
            )
        } else {
            format!(
                "{kind_label} requires a {} endpoint; the catalog direction is {}",
                need.as_str(),
                endpoint.direction.as_str()
            )
        };
        return PinReport {
            status: RouteStatus::Unsupported,
            direction: endpoint.direction.as_str(),
            label: Some(endpoint.label.clone()),
            state: endpoint.state.as_str(),
            format,
            reason,
        };
    }
    let reason = match classify_format(endpoint.format) {
        FormatClass::Native => None,
        FormatClass::Missing => {
            Some("the catalog does not include a mix format for this endpoint".into())
        }
        FormatClass::OutsideSubset => {
            Some("the mix format is outside the native 48 kHz stereo float32 subset".into())
        }
        FormatClass::OutsideMask => Some(
            "the channel mask is outside the unspecified or front-left/front-right subset".into(),
        ),
    };
    if let Some(reason) = reason {
        return PinReport {
            status: RouteStatus::Unsupported,
            direction: endpoint.direction.as_str(),
            label: Some(endpoint.label.clone()),
            state: endpoint.state.as_str(),
            format,
            reason,
        };
    }
    let reason = if role == "output" {
        "the active endpoint matches the saved output id and native render format".into()
    } else {
        "the active endpoint matches the saved source id, source kind, and native format".into()
    };
    PinReport {
        status: RouteStatus::Eligible,
        direction: endpoint.direction.as_str(),
        label: Some(endpoint.label.clone()),
        state: endpoint.state.as_str(),
        format,
        reason,
    }
}

fn native_sample_body(format: &EndpointFormat) -> bool {
    format.sample_rate_hz == 48_000
        && format.channels == 2
        && format.bits_per_sample == 32
        && format.valid_bits_per_sample == 32
        && format.block_align == 8
        && format.float32
}

fn classify_format(format: Option<EndpointFormat>) -> FormatClass {
    let Some(format) = format else {
        return FormatClass::Missing;
    };
    if is_native_lab_format(&format) {
        FormatClass::Native
    } else if native_sample_body(&format) {
        FormatClass::OutsideMask
    } else {
        FormatClass::OutsideSubset
    }
}

fn format_text(format: Option<EndpointFormat>) -> String {
    match format {
        None => "none".into(),
        Some(format) => format!(
            "{} Hz, {} ch, {}/{} bits, mask {}, {}",
            format.sample_rate_hz,
            format.channels,
            format.valid_bits_per_sample,
            format.bits_per_sample,
            format.channel_mask,
            if format.float32 {
                "float32"
            } else {
                "not-float32"
            }
        ),
    }
}

fn staging_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".rlb-profile.tmp");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

fn stage_bytes(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let staged = staging_path(path);
    if staged.exists() {
        return Err(format!(
            "staging file {} already exists; profile {} was not modified",
            staged.display(),
            path.display()
        ));
    }
    if path
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty() && !parent.exists())
    {
        return Err(format!(
            "directory for {} is absent; nothing was written",
            path.display()
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|error| {
            format!(
                "unable to stage {}: {error}; profile {} was not modified",
                staged.display(),
                path.display()
            )
        })?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "unable to finish staging {}: {error}; profile {} was not modified",
            staged.display(),
            path.display()
        ));
    }
    Ok(staged)
}

fn publish_staged(from: &Path, to: &Path, replace_existing: bool) -> std::io::Result<()> {
    let from_wide = wide_path(from);
    let to_wide = wide_path(to);
    const MOVEFILE_REPLACE_EXISTING: u32 = 1;
    const MOVEFILE_WRITE_THROUGH: u32 = 8;
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace_existing {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    let published = unsafe {
        // SAFETY: both buffers are NUL-terminated and borrowed for this call only.
        MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags)
    };
    if published == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, new_name: *const u16, flags: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn sample() -> RouteProfile {
        RouteProfile::from_selection(
            "source-fixed-id",
            SourceKind::RenderLoopback,
            Some("Saved Speaker"),
            "output-fixed-id",
            Some("Saved Headphones"),
            SavedPlayback::remote_defaults(),
        )
        .unwrap()
    }

    fn native_format() -> EndpointFormat {
        EndpointFormat {
            sample_rate_hz: 48_000,
            channels: 2,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            block_align: 8,
            channel_mask: 3,
            float32: true,
        }
    }

    fn endpoint(
        id: &str,
        label: &str,
        direction: CatalogDirection,
        state: CatalogState,
        format: Option<EndpointFormat>,
    ) -> CatalogEndpoint {
        CatalogEndpoint {
            id: id.to_owned(),
            label: label.to_owned(),
            direction,
            state,
            format,
        }
    }

    fn replace_once(text: &str, from: &str, to: &str) -> String {
        assert!(
            text.contains(from),
            "pattern {from} missing in profile text:\n{text}"
        );
        text.replacen(from, to, 1)
    }

    fn insert_top_level_field(text: &str, field: &str) -> String {
        let trimmed = text.trim_end();
        let body = trimmed
            .strip_suffix('}')
            .expect("profile json ends with an object");
        format!("{body},{field}}}\n")
    }

    #[test]
    fn round_trip_defaults_remote_monitoring_and_rejects_invalid_profiles() {
        let profile = sample();
        assert_eq!(profile.playback().mode, PlaybackMode::Remote);
        assert!(!profile.playback().mode.original_monitoring());
        let text = String::from_utf8(profile.to_json_bytes().unwrap()).unwrap();
        assert!(text.contains("\"schema\": 1"));
        assert!(!text.contains("index"));
        let parsed = parse_profile(&text).unwrap();
        assert_eq!(parsed, profile);
        assert!(parsed.inspection_report().contains("playback=remote"));
        assert!(
            parsed
                .inspection_report()
                .contains("original_monitoring=off")
        );
        assert!(
            parsed
                .inspection_report()
                .contains("device_enumeration=not_used")
        );
        assert!(parsed.inspection_report().contains("stream_open=not_used"));

        let cases = [
            (
                replace_once(&text, "\"schema\": 1", "\"schema\": 2"),
                "schema version 2 is unsupported",
            ),
            (
                insert_top_level_field(&text, "\"index\":0"),
                "unknown field",
            ),
            (
                replace_once(&text, "\"schema\": 1,", "\"schema\": 1,\"schema\": 1,"),
                "duplicate field `schema`",
            ),
            (
                replace_once(&text, "source-fixed-id", ""),
                "source endpoint id is empty",
            ),
            (
                replace_once(&text, "source-fixed-id", " source-fixed-id"),
                "exact opaque characters",
            ),
            (
                replace_once(&text, "render_loopback", "microphone"),
                "source kind must be render_loopback or capture_endpoint",
            ),
            (
                replace_once(&text, "\"local_gain\": 1.0", "\"local_gain\": 1.5"),
                "local_gain must be finite in 0..1",
            ),
            (
                replace_once(&text, "\"local_gain\": 1.0", "\"local_gain\": 1e999"),
                "number out of range",
            ),
            (
                replace_once(&text, "\"buffer_periods\": 2", "\"buffer_periods\": 0"),
                "buffer periods must be 1..8",
            ),
            (
                replace_once(&text, "\"buffer_periods\": 2", "\"buffer_periods\": 9"),
                "buffer periods must be 1..8",
            ),
            (
                replace_once(
                    &text,
                    "\"playback\": \"remote\"",
                    "\"playback\": \"sideways\"",
                ),
                "play must be remote, local, mix or mute",
            ),
        ];
        for (document, expected) in cases {
            let error = parse_profile(&document).expect_err(&document);
            assert!(
                error.contains(expected),
                "error `{error}` did not contain `{expected}`"
            );
        }
    }

    #[test]
    fn failed_write_preserves_the_existing_valid_profile() {
        let dir = scratch_dir();
        let path = dir.join("route.json");
        let profile = sample();
        create_profile(&path, &profile).unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(load_profile(&path).unwrap(), profile);
        assert!(!staging_path(&path).exists());

        let blocked = create_profile(&path, &profile).unwrap_err();
        assert!(blocked.contains("was not modified"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!staging_path(&path).exists());

        fs::write(staging_path(&path), b"occupied").unwrap();
        let mut updated = profile.clone();
        updated.buffer_periods = 4;
        let failed = replace_profile(&path, &updated).unwrap_err();
        assert!(failed.contains("was not modified"), "{failed}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(staging_path(&path)).unwrap(), b"occupied");
        fs::remove_file(staging_path(&path)).unwrap();

        replace_profile(&path, &updated).unwrap();
        assert_eq!(load_profile(&path).unwrap().playback().buffer_periods, 4);
        assert!(!staging_path(&path).exists());
        let published = fs::read(&path).unwrap();
        assert_ne!(published, before);
        assert!(published.ends_with(b"\n"));
    }

    #[test]
    fn synthetic_catalog_pins_identity_through_reorder_rename_and_rejection() {
        let profile = sample();
        let source = endpoint(
            "source-fixed-id",
            "Current Speaker",
            CatalogDirection::Render,
            CatalogState::Active,
            Some(native_format()),
        );
        let output = endpoint(
            "output-fixed-id",
            "Current Headphones",
            CatalogDirection::Render,
            CatalogState::Active,
            Some(native_format()),
        );
        let forward = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
            source.clone(),
            output.clone(),
        ]));
        let reversed = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
            output.clone(),
            source.clone(),
        ]));
        for report in [&forward, &reversed] {
            assert!(report.contains("source_status=eligible"));
            assert!(report.contains("output_status=eligible"));
            assert!(report.contains("route_status=eligible"));
            assert!(report.contains("separation=distinct"));
            assert!(report.contains("negotiation=not_attempted"));
            assert!(report.contains("source_current_label=Current Speaker"));
            assert!(report.contains("source_label=Saved Speaker"));
            assert!(report.contains("output_current_label=Current Headphones"));
        }

        let renamed = endpoint(
            "source-fixed-id",
            "Renamed Speaker",
            CatalogDirection::Render,
            CatalogState::Active,
            Some(native_format()),
        );
        let rename_report =
            profile.preflight_report(&RouteCatalog::from_endpoints(vec![renamed, output.clone()]));
        assert!(rename_report.contains("source_status=eligible"));
        assert!(rename_report.contains("source_current_label=Renamed Speaker"));
        assert!(rename_report.contains("source_label=Saved Speaker"));
        assert!(rename_report.contains("source_pinned_id=source-fixed-id"));

        let decoy = endpoint(
            "decoy-same-label-id",
            "Saved Speaker",
            CatalogDirection::Render,
            CatalogState::Active,
            Some(native_format()),
        );
        let absent = profile.preflight_report(&RouteCatalog::from_endpoints(vec![decoy, output]));
        assert!(absent.contains("source_status=unavailable"));
        assert!(absent.contains("source_state=absent"));
        assert!(absent.contains("saved label is not an identity"));
        assert!(absent.contains("source_pinned_id=source-fixed-id"));
        assert!(!absent.contains("decoy-same-label-id"));
        assert!(absent.contains("route_status=unavailable"));

        for (state, token) in [
            (CatalogState::Disabled, "disabled"),
            (CatalogState::Unplugged, "unplugged"),
            (CatalogState::NotPresent, "not_present"),
        ] {
            let held = endpoint(
                "source-fixed-id",
                "Held Speaker",
                CatalogDirection::Render,
                state,
                Some(native_format()),
            );
            let report = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
                held,
                endpoint(
                    "output-fixed-id",
                    "Current Headphones",
                    CatalogDirection::Render,
                    CatalogState::Active,
                    Some(native_format()),
                ),
            ]));
            assert!(
                report.contains("source_status=unavailable"),
                "{token}\n{report}"
            );
            assert!(
                report.contains(&format!("source_state={token}")),
                "{report}"
            );
            assert!(report.contains("stays pinned"), "{report}");
            assert!(
                report.contains("source_current_label=Held Speaker"),
                "{report}"
            );
        }

        let capture_profile = RouteProfile::from_selection(
            "source-fixed-id",
            SourceKind::CaptureEndpoint,
            Some("Saved Mic"),
            "output-fixed-id",
            Some("Saved Headphones"),
            SavedPlayback::remote_defaults(),
        )
        .unwrap();
        let wrong_direction =
            capture_profile.preflight_report(&RouteCatalog::from_endpoints(vec![
                endpoint(
                    "source-fixed-id",
                    "A Render Device",
                    CatalogDirection::Render,
                    CatalogState::Active,
                    Some(native_format()),
                ),
                endpoint(
                    "output-fixed-id",
                    "Current Headphones",
                    CatalogDirection::Render,
                    CatalogState::Active,
                    Some(native_format()),
                ),
            ]));
        assert!(wrong_direction.contains("source_status=unsupported"));
        assert!(wrong_direction.contains("source_direction=render"));
        assert!(wrong_direction.contains("capture_endpoint requires a capture endpoint"));

        let mut slow = native_format();
        slow.sample_rate_hz = 44_100;
        let bad_format = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
            endpoint(
                "source-fixed-id",
                "Slow Speaker",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(slow),
            ),
            endpoint(
                "output-fixed-id",
                "Current Headphones",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(native_format()),
            ),
        ]));
        assert!(bad_format.contains("source_status=unsupported"));
        assert!(bad_format.contains("44100 Hz"));
        assert!(bad_format.contains("outside the native 48 kHz stereo float32 subset"));

        let mut mask = native_format();
        mask.channel_mask = 1;
        let bad_mask = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
            endpoint(
                "source-fixed-id",
                "Masked Speaker",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(native_format()),
            ),
            endpoint(
                "output-fixed-id",
                "Masked Output",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(mask),
            ),
        ]));
        assert!(bad_mask.contains("output_status=unsupported"));
        assert!(bad_mask.contains("channel mask is outside"));

        let output_capture = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
            endpoint(
                "source-fixed-id",
                "Current Speaker",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(native_format()),
            ),
            endpoint(
                "output-fixed-id",
                "A Microphone",
                CatalogDirection::Capture,
                CatalogState::Active,
                Some(native_format()),
            ),
        ]));
        assert!(output_capture.contains("output_status=unsupported"));
        assert!(output_capture.contains("the output must be a render endpoint"));

        let shared = RouteProfile::from_selection(
            "shared-endpoint-id",
            SourceKind::RenderLoopback,
            Some("Same Device"),
            "shared-endpoint-id",
            Some("Same Device"),
            SavedPlayback::remote_defaults(),
        )
        .unwrap();
        let same = shared.preflight_report(&RouteCatalog::from_endpoints(vec![endpoint(
            "shared-endpoint-id",
            "Same Device",
            CatalogDirection::Render,
            CatalogState::Active,
            Some(native_format()),
        )]));
        assert!(same.contains("source_status=eligible"));
        assert!(same.contains("output_status=eligible"));
        assert!(same.contains("separation=same_endpoint"));
        assert!(same.contains("route_status=unsupported"));

        let duplicate = profile.preflight_report(&RouteCatalog::from_endpoints(vec![
            endpoint(
                "source-fixed-id",
                "First",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(native_format()),
            ),
            endpoint(
                "source-fixed-id",
                "Second",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(native_format()),
            ),
            endpoint(
                "output-fixed-id",
                "Current Headphones",
                CatalogDirection::Render,
                CatalogState::Active,
                Some(native_format()),
            ),
        ]));
        assert!(duplicate.contains("source_status=unsupported"));
        assert!(duplicate.contains("more than once"));
        assert!(duplicate.contains("source_current_label=-"));
        assert!(!duplicate.contains("source_current_label=First"));
    }

    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch_dir() -> Scratch {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rlb-c01-profile-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}
