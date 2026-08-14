//! Content-free `DSpark` degeneration detection over cumulative Prometheus counters.
//!
//! This module deliberately owns no HTTP, routing, configuration, logging, or
//! metrics integration. Callers provide bounded Prometheus payloads and an
//! already-attested opaque engine-incarnation commitment. A threshold crossing
//! is only a signal: observe-only integrations may record it, while enforcing
//! integrations must explicitly call [`DsparkGuard::quarantine`].

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

pub const MAX_PROMETHEUS_BYTES: usize = 4 << 20;
const MAX_PROMETHEUS_LINES: usize = 65_536;
const MAX_PROMETHEUS_LINE_BYTES: usize = 16 << 10;
const MAX_TARGET_SERIES: usize = 4_096;
const MAX_LABELS_PER_SERIES: usize = 32;
const MAX_LABEL_NAME_BYTES: usize = 128;
const MAX_LABEL_VALUE_BYTES: usize = 1_024;
const MAX_EXPECTED_POSITIONS: usize = 64;
const MAX_EXACT_COUNTER: f64 = 9_007_199_254_740_992.0;

const DRAFT_STEPS: &str = "vllm:spec_decode_num_drafts_total";
const PROPOSED_TOKENS: &str = "vllm:spec_decode_num_draft_tokens_total";
const ACCEPTED_TOKENS: &str = "vllm:spec_decode_num_accepted_tokens_total";
const ACCEPTED_PER_POSITION: &str = "vllm:spec_decode_num_accepted_tokens_per_pos_total";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DsparkCounters {
    pub draft_steps: u64,
    pub proposed_tokens: u64,
    pub accepted_tokens: u64,
    pub accepted_per_position: Box<[u64]>,
    series_identity: Box<[SeriesKey]>,
}

impl DsparkCounters {
    #[cfg(test)]
    pub(crate) fn synthetic(
        draft_steps: u64,
        proposed_tokens: u64,
        accepted_tokens: u64,
        accepted_per_position: Box<[u64]>,
    ) -> Self {
        Self {
            draft_steps,
            proposed_tokens,
            accepted_tokens,
            accepted_per_position,
            series_identity: Box::new([]),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseFailure {
    Oversized,
    InvalidUtf8,
    TooManyLines,
    LineTooLong,
    InvalidExpectedPositions,
    Malformed,
    Missing,
    Partial,
    NonFinite,
    Duplicate,
    MultipleSeries,
    MismatchedLabels,
    UnexpectedPosition,
    Capacity,
    Overflow,
}

impl ParseFailure {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::TooManyLines => "too_many_lines",
            Self::LineTooLong => "line_too_long",
            Self::InvalidExpectedPositions => "invalid_expected_positions",
            Self::Malformed => "malformed",
            Self::Missing => "missing",
            Self::Partial => "partial",
            Self::NonFinite => "non_finite",
            Self::Duplicate => "duplicate",
            Self::MultipleSeries => "multiple_series",
            Self::MismatchedLabels => "mismatched_labels",
            Self::UnexpectedPosition => "unexpected_position",
            Self::Capacity => "capacity",
            Self::Overflow => "overflow",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowOutcome {
    Baseline,
    Clean,
    ZeroAcceptance,
    Idle,
    CounterReset,
    Inconsistent,
    Parse(ParseFailure),
}

impl WindowOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Clean => "clean",
            Self::ZeroAcceptance => "zero_acceptance",
            Self::Idle => "idle",
            Self::CounterReset => "counter_reset",
            Self::Inconsistent => "inconsistent",
            Self::Parse(failure) => failure.label(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub outcome: WindowOutcome,
    pub consecutive_zero_windows: usize,
    pub threshold_met: bool,
    pub newly_met: bool,
    pub quarantined: bool,
    pub measurement: Option<WindowMeasurement>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowMeasurement {
    pub draft_steps: u64,
    pub proposed_tokens: u64,
    pub accepted_tokens: u64,
    pub accepted_per_position: Box<[u64]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardConfigError {
    ExpectedPositions,
    ConsecutiveWindows,
    MinimumProposedTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuarantineOutcome {
    Entered,
    AlreadyQuarantined,
    ThresholdNotMet,
    IncarnationUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IncarnationOutcome {
    Established,
    Unchanged,
    Changed,
    Rearmed,
}

/// Opaque commitment returned only after the caller validates engine identity.
///
/// It is intentionally redacted from `Debug` and offers no accessor. The guard
/// needs equality only; callers remain responsible for constructing it from an
/// authenticated and compatibility-attested identity document.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct AttestedEngineCoreIncarnation([u8; 32]);

impl AttestedEngineCoreIncarnation {
    #[must_use]
    pub const fn from_commitment(commitment: [u8; 32]) -> Self {
        Self(commitment)
    }
}

impl fmt::Debug for AttestedEngineCoreIncarnation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

pub struct DsparkGuard {
    expected_positions: usize,
    required_zero_windows: usize,
    minimum_proposed_tokens: u64,
    previous: Option<DsparkCounters>,
    consecutive_zero_windows: usize,
    threshold_met: bool,
    quarantined: bool,
    incarnation: Option<AttestedEngineCoreIncarnation>,
}

impl DsparkGuard {
    /// Create a detector with explicit K-width, work, and duration thresholds.
    ///
    /// # Errors
    ///
    /// Rejects zero or unbounded dimensions. Configuration validation is kept
    /// here so an integration cannot accidentally create a one-sample or
    /// zero-work trip condition.
    pub fn new(
        expected_positions: usize,
        required_zero_windows: usize,
        minimum_proposed_tokens: u64,
    ) -> Result<Self, GuardConfigError> {
        if !(1..=MAX_EXPECTED_POSITIONS).contains(&expected_positions) {
            return Err(GuardConfigError::ExpectedPositions);
        }
        if required_zero_windows < 2 {
            return Err(GuardConfigError::ConsecutiveWindows);
        }
        if minimum_proposed_tokens == 0 {
            return Err(GuardConfigError::MinimumProposedTokens);
        }
        Ok(Self {
            expected_positions,
            required_zero_windows,
            minimum_proposed_tokens,
            previous: None,
            consecutive_zero_windows: 0,
            threshold_met: false,
            quarantined: false,
            incarnation: None,
        })
    }

    #[must_use]
    pub const fn quarantined(&self) -> bool {
        self.quarantined
    }

    /// Observe one payload. Any rejected payload breaks consecutiveness and
    /// clears the counter baseline, so a later scrape cannot bridge an unknown
    /// interval and satisfy the threshold.
    pub fn observe_prometheus(&mut self, body: &[u8]) -> Observation {
        let sample = parse_prometheus(body, self.expected_positions);
        self.observe(sample)
    }

    /// Observe a pre-parsed sample, useful for polling integrations and tests.
    pub fn observe(&mut self, sample: Result<DsparkCounters, ParseFailure>) -> Observation {
        let Ok(sample) = sample else {
            let failure = sample.expect_err("checked error");
            self.previous = None;
            self.clear_streak();
            return self.observation(WindowOutcome::Parse(failure), false, None);
        };
        if sample.accepted_per_position.len() != self.expected_positions {
            self.previous = None;
            self.clear_streak();
            return self.observation(
                WindowOutcome::Parse(ParseFailure::InvalidExpectedPositions),
                false,
                None,
            );
        }
        let Some(previous) = self.previous.replace(sample.clone()) else {
            self.clear_streak();
            return self.observation(WindowOutcome::Baseline, false, None);
        };
        if sample.series_identity != previous.series_identity {
            self.previous = None;
            self.clear_streak();
            return self.observation(WindowOutcome::Inconsistent, false, None);
        }
        let Some(draft_steps) = sample.draft_steps.checked_sub(previous.draft_steps) else {
            self.clear_streak();
            return self.observation(WindowOutcome::CounterReset, false, None);
        };
        let Some(proposed_tokens) = sample.proposed_tokens.checked_sub(previous.proposed_tokens)
        else {
            self.clear_streak();
            return self.observation(WindowOutcome::CounterReset, false, None);
        };
        let Some(accepted_tokens) = sample.accepted_tokens.checked_sub(previous.accepted_tokens)
        else {
            self.clear_streak();
            return self.observation(WindowOutcome::CounterReset, false, None);
        };
        let mut position_sum = 0_u64;
        let mut all_positions_zero = true;
        let mut position_deltas = Vec::with_capacity(self.expected_positions);
        for (&current, &prior) in sample
            .accepted_per_position
            .iter()
            .zip(previous.accepted_per_position.iter())
        {
            let Some(delta) = current.checked_sub(prior) else {
                self.clear_streak();
                return self.observation(WindowOutcome::CounterReset, false, None);
            };
            all_positions_zero &= delta == 0;
            position_deltas.push(delta);
            let Some(next) = position_sum.checked_add(delta) else {
                self.previous = None;
                self.clear_streak();
                return self.observation(WindowOutcome::Inconsistent, false, None);
            };
            position_sum = next;
        }
        if accepted_tokens > proposed_tokens || position_sum != accepted_tokens {
            self.previous = None;
            self.clear_streak();
            return self.observation(WindowOutcome::Inconsistent, false, None);
        }
        let measurement = Some(WindowMeasurement {
            draft_steps,
            proposed_tokens,
            accepted_tokens,
            accepted_per_position: position_deltas.into_boxed_slice(),
        });
        if draft_steps == 0 || proposed_tokens < self.minimum_proposed_tokens {
            self.clear_streak();
            return self.observation(WindowOutcome::Idle, false, measurement);
        }
        if accepted_tokens != 0 || !all_positions_zero {
            self.clear_streak();
            return self.observation(WindowOutcome::Clean, false, measurement);
        }

        self.consecutive_zero_windows = self
            .consecutive_zero_windows
            .saturating_add(1)
            .min(self.required_zero_windows);
        let newly_met =
            !self.threshold_met && self.consecutive_zero_windows == self.required_zero_windows;
        self.threshold_met = self.consecutive_zero_windows == self.required_zero_windows;
        self.observation(WindowOutcome::ZeroAcceptance, newly_met, measurement)
    }

    /// Enter sticky quarantine after the detector threshold is active.
    ///
    /// A current attested incarnation is mandatory because otherwise no future
    /// caller could prove that a replacement process, rather than the same bad
    /// process, is requesting re-admission.
    pub fn quarantine(&mut self) -> QuarantineOutcome {
        if self.quarantined {
            return QuarantineOutcome::AlreadyQuarantined;
        }
        if !self.threshold_met {
            return QuarantineOutcome::ThresholdNotMet;
        }
        if self.incarnation.is_none() {
            return QuarantineOutcome::IncarnationUnavailable;
        }
        self.quarantined = true;
        QuarantineOutcome::Entered
    }

    /// Publish a compatibility-attested engine incarnation commitment.
    ///
    /// Only a changed commitment can clear sticky quarantine. A change also
    /// clears the cumulative baseline, preventing deltas from crossing process
    /// generations.
    pub fn attest_incarnation(
        &mut self,
        incarnation: AttestedEngineCoreIncarnation,
    ) -> IncarnationOutcome {
        let Some(previous) = self.incarnation else {
            self.incarnation = Some(incarnation);
            self.previous = None;
            self.clear_streak();
            return IncarnationOutcome::Established;
        };
        if previous == incarnation {
            return IncarnationOutcome::Unchanged;
        }
        self.incarnation = Some(incarnation);
        self.previous = None;
        self.clear_streak();
        if self.quarantined {
            self.quarantined = false;
            IncarnationOutcome::Rearmed
        } else {
            IncarnationOutcome::Changed
        }
    }

    /// Restore a quarantine that was durably committed before this process
    /// started. Callers must validate the store and upstream ordinal mapping
    /// before invoking this method.
    pub fn restore_quarantine(&mut self, incarnation: AttestedEngineCoreIncarnation) {
        self.incarnation = Some(incarnation);
        self.previous = None;
        self.clear_streak();
        self.quarantined = true;
    }

    fn clear_streak(&mut self) {
        self.consecutive_zero_windows = 0;
        self.threshold_met = false;
    }

    fn observation(
        &self,
        outcome: WindowOutcome,
        newly_met: bool,
        measurement: Option<WindowMeasurement>,
    ) -> Observation {
        Observation {
            outcome,
            consecutive_zero_windows: self.consecutive_zero_windows,
            threshold_met: self.threshold_met,
            newly_met,
            quarantined: self.quarantined,
            measurement,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum MetricKind {
    DraftSteps,
    ProposedTokens,
    AcceptedTokens,
    AcceptedPerPosition,
}

impl MetricKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            DRAFT_STEPS => Some(Self::DraftSteps),
            PROPOSED_TOKENS => Some(Self::ProposedTokens),
            ACCEPTED_TOKENS => Some(Self::AcceptedTokens),
            ACCEPTED_PER_POSITION => Some(Self::AcceptedPerPosition),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct SeriesKey {
    kind: MetricKind,
    labels: Vec<(String, String)>,
}

/// Parse only the four `DSpark` counter families needed by the guard.
///
/// Unknown Prometheus families are ignored. Target families are strict:
/// malformed, duplicate, partial, non-finite, or unexpected-position series
/// reject the entire sample rather than yielding a potentially unsafe zero.
///
/// # Errors
///
/// Returns a fixed, content-free failure when the payload or required metric
/// families violate any syntax, completeness, uniqueness, size, or numeric
/// bound.
pub fn parse_prometheus(
    body: &[u8],
    expected_positions: usize,
) -> Result<DsparkCounters, ParseFailure> {
    if body.len() > MAX_PROMETHEUS_BYTES {
        return Err(ParseFailure::Oversized);
    }
    if !(1..=MAX_EXPECTED_POSITIONS).contains(&expected_positions) {
        return Err(ParseFailure::InvalidExpectedPositions);
    }
    let text = std::str::from_utf8(body).map_err(|_| ParseFailure::InvalidUtf8)?;
    let mut lines = 0_usize;
    let mut target_series = 0_usize;
    let mut seen_any = false;
    let mut seen = HashSet::new();
    let mut series_identity = Vec::new();
    let mut cohort_labels: Option<Vec<(String, String)>> = None;
    let mut totals = [None; 3];
    let mut positions = vec![None; expected_positions];

    for raw_line in text.lines() {
        lines += 1;
        if lines > MAX_PROMETHEUS_LINES {
            return Err(ParseFailure::TooManyLines);
        }
        if raw_line.len() > MAX_PROMETHEUS_LINE_BYTES {
            return Err(ParseFailure::LineTooLong);
        }
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name_end = line
            .find(|character: char| character == '{' || character.is_ascii_whitespace())
            .unwrap_or(line.len());
        let Some(kind) = MetricKind::from_name(&line[..name_end]) else {
            continue;
        };
        seen_any = true;
        target_series += 1;
        if target_series > MAX_TARGET_SERIES {
            return Err(ParseFailure::Capacity);
        }
        let (labels, value) = parse_target_series(&line[name_end..])?;
        let key = SeriesKey {
            kind,
            labels: labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        if !seen.insert(key.clone()) {
            return Err(ParseFailure::Duplicate);
        }
        series_identity.push(key);
        match kind {
            MetricKind::DraftSteps => set_single(&mut totals[0], value)?,
            MetricKind::ProposedTokens => set_single(&mut totals[1], value)?,
            MetricKind::AcceptedTokens => set_single(&mut totals[2], value)?,
            MetricKind::AcceptedPerPosition => {
                let Some(raw_position) = labels.get("position") else {
                    return Err(ParseFailure::Malformed);
                };
                let position = raw_position
                    .parse::<usize>()
                    .map_err(|_| ParseFailure::Malformed)?;
                let Some(total) = positions.get_mut(position) else {
                    return Err(ParseFailure::UnexpectedPosition);
                };
                set_single(total, value)?;
            }
        }
        validate_cohort_labels(&mut cohort_labels, &labels)?;
    }
    if !seen_any {
        return Err(ParseFailure::Missing);
    }
    let [
        Some(draft_steps),
        Some(proposed_tokens),
        Some(accepted_tokens),
    ] = totals
    else {
        return Err(ParseFailure::Partial);
    };
    if positions.iter().any(Option::is_none) {
        return Err(ParseFailure::Partial);
    }
    let accepted_per_position = positions
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(ParseFailure::Partial)?
        .into_boxed_slice();
    series_identity.sort_unstable();
    Ok(DsparkCounters {
        draft_steps,
        proposed_tokens,
        accepted_tokens,
        accepted_per_position,
        series_identity: series_identity.into_boxed_slice(),
    })
}

fn validate_cohort_labels(
    cohort: &mut Option<Vec<(String, String)>>,
    labels: &BTreeMap<String, String>,
) -> Result<(), ParseFailure> {
    let base = labels
        .iter()
        .filter(|(name, _)| name.as_str() != "position")
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Vec<_>>();
    match cohort {
        Some(expected) if expected != &base => Err(ParseFailure::MismatchedLabels),
        None => {
            *cohort = Some(base);
            Ok(())
        }
        Some(_) => Ok(()),
    }
}

fn set_single(total: &mut Option<u64>, value: u64) -> Result<(), ParseFailure> {
    if total.replace(value).is_some() {
        return Err(ParseFailure::MultipleSeries);
    }
    Ok(())
}

fn parse_target_series(rest: &str) -> Result<(BTreeMap<String, String>, u64), ParseFailure> {
    let mut rest = rest.trim_start();
    let labels = if let Some(label_body) = rest.strip_prefix('{') {
        let end = quoted_closing_brace(label_body).ok_or(ParseFailure::Malformed)?;
        let labels = parse_labels(&label_body[..end])?;
        rest = &label_body[end + 1..];
        labels
    } else {
        BTreeMap::new()
    };
    let mut fields = rest.split_ascii_whitespace();
    let value = fields.next().ok_or(ParseFailure::Malformed)?;
    if fields.next().is_some() {
        return Err(ParseFailure::Malformed);
    }
    Ok((labels, parse_counter(value)?))
}

fn quoted_closing_brace(input: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == '}' && !quoted {
            return Some(index);
        }
    }
    None
}

fn parse_labels(input: &str) -> Result<BTreeMap<String, String>, ParseFailure> {
    let mut labels = BTreeMap::new();
    let mut rest = input.trim();
    while !rest.is_empty() {
        if labels.len() >= MAX_LABELS_PER_SERIES {
            return Err(ParseFailure::Capacity);
        }
        let equals = rest.find('=').ok_or(ParseFailure::Malformed)?;
        let name = rest[..equals].trim();
        if name.is_empty()
            || name.len() > MAX_LABEL_NAME_BYTES
            || !name
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            return Err(ParseFailure::Malformed);
        }
        rest = rest[equals + 1..].trim_start();
        let Some(quoted) = rest.strip_prefix('"') else {
            return Err(ParseFailure::Malformed);
        };
        let (value, remaining) = parse_quoted_value(quoted)?;
        if value.len() > MAX_LABEL_VALUE_BYTES || labels.insert(name.to_owned(), value).is_some() {
            return Err(ParseFailure::Duplicate);
        }
        rest = remaining.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some(after_comma) = rest.strip_prefix(',') else {
            return Err(ParseFailure::Malformed);
        };
        rest = after_comma.trim_start();
        if rest.is_empty() {
            return Err(ParseFailure::Malformed);
        }
    }
    Ok(labels)
}

fn parse_quoted_value(input: &str) -> Result<(String, &str), ParseFailure> {
    let mut value = String::new();
    let mut chars = input.char_indices();
    while let Some((index, character)) = chars.next() {
        match character {
            '"' => return Ok((value, &input[index + 1..])),
            '\\' => {
                let Some((_, escaped)) = chars.next() else {
                    return Err(ParseFailure::Malformed);
                };
                value.push(match escaped {
                    '\\' => '\\',
                    '"' => '"',
                    'n' => '\n',
                    _ => return Err(ParseFailure::Malformed),
                });
            }
            _ => value.push(character),
        }
        if value.len() > MAX_LABEL_VALUE_BYTES {
            return Err(ParseFailure::Capacity);
        }
    }
    Err(ParseFailure::Malformed)
}

fn parse_counter(value: &str) -> Result<u64, ParseFailure> {
    let number = value.parse::<f64>().map_err(|_| ParseFailure::Malformed)?;
    if !number.is_finite() {
        return Err(ParseFailure::NonFinite);
    }
    if number < 0.0 || number.fract() != 0.0 || number > MAX_EXACT_COUNTER {
        return Err(ParseFailure::Malformed);
    }
    number
        .to_string()
        .parse::<u64>()
        .map_err(|_| ParseFailure::Malformed)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn body(drafts: u64, proposed: u64, accepted: u64, positions: &[u64]) -> Vec<u8> {
        let mut output = format!(
            "# HELP ignored x\n{DRAFT_STEPS} {drafts}\n{PROPOSED_TOKENS} {proposed}\n{ACCEPTED_TOKENS} {accepted}\n"
        );
        for (position, value) in positions.iter().enumerate() {
            writeln!(
                output,
                "{ACCEPTED_PER_POSITION}{{position=\"{position}\"}} {value}"
            )
            .unwrap();
        }
        output.into_bytes()
    }

    fn counters(drafts: u64, proposed: u64, accepted: u64, positions: &[u64]) -> DsparkCounters {
        DsparkCounters::synthetic(drafts, proposed, accepted, positions.into())
    }

    fn guard() -> DsparkGuard {
        DsparkGuard::new(5, 3, 100).unwrap()
    }

    fn incarnation(byte: u8) -> AttestedEngineCoreIncarnation {
        AttestedEngineCoreIncarnation::from_commitment([byte; 32])
    }

    #[test]
    fn parser_rejects_multiple_labeled_series_instead_of_hiding_shard_resets() {
        let body = format!(
            "{DRAFT_STEPS}{{engine=\"0\"}} 4\n\
             {DRAFT_STEPS}{{engine=\"1\"}} 6\n\
             {PROPOSED_TOKENS}{{engine=\"0\"}} 20\n\
             {PROPOSED_TOKENS}{{engine=\"1\"}} 30\n\
             {ACCEPTED_TOKENS}{{engine=\"0\"}} 5\n\
             {ACCEPTED_TOKENS}{{engine=\"1\"}} 7\n\
             {ACCEPTED_PER_POSITION}{{engine=\"0\",position=\"0\"}} 3\n\
             {ACCEPTED_PER_POSITION}{{position=\"0\",engine=\"1\"}} 4\n\
             {ACCEPTED_PER_POSITION}{{position=\"1\"}} 5\n"
        );
        assert_eq!(
            parse_prometheus(body.as_bytes(), 2),
            Err(ParseFailure::MultipleSeries)
        );
    }

    #[test]
    fn parser_requires_one_coherent_label_domain_and_canonicalizes_label_order() {
        let mismatched = format!(
            "{DRAFT_STEPS}{{engine=\"a\"}} 1\n\
             {PROPOSED_TOKENS}{{engine=\"b\"}} 100\n\
             {ACCEPTED_TOKENS}{{engine=\"a\"}} 0\n\
             {ACCEPTED_PER_POSITION}{{engine=\"a\",position=\"0\"}} 0\n"
        );
        assert_eq!(
            parse_prometheus(mismatched.as_bytes(), 1),
            Err(ParseFailure::MismatchedLabels)
        );

        let coherent = format!(
            "{DRAFT_STEPS}{{engine=\"a\",model=\"m\"}} 1\n\
             {PROPOSED_TOKENS}{{model=\"m\",engine=\"a\"}} 100\n\
             {ACCEPTED_TOKENS}{{engine=\"a\",model=\"m\"}} 0\n\
             {ACCEPTED_PER_POSITION}{{position=\"0\",model=\"m\",engine=\"a\"}} 0\n"
        );
        let parsed = parse_prometheus(coherent.as_bytes(), 1).unwrap();
        assert_eq!(parsed.draft_steps, 1);
        assert_eq!(parsed.proposed_tokens, 100);
        assert_eq!(parsed.accepted_tokens, 0);
        assert_eq!(parsed.accepted_per_position.as_ref(), &[0]);
    }

    #[test]
    fn changed_series_identity_breaks_the_baseline() {
        let unlabeled = body(0, 0, 0, &[0]);
        let labeled = format!(
            "{DRAFT_STEPS}{{engine=\"a\"}} 20\n\
             {PROPOSED_TOKENS}{{engine=\"a\"}} 100\n\
             {ACCEPTED_TOKENS}{{engine=\"a\"}} 0\n\
             {ACCEPTED_PER_POSITION}{{engine=\"a\",position=\"0\"}} 0\n"
        );
        let mut guard = DsparkGuard::new(1, 3, 100).unwrap();
        assert_eq!(
            guard.observe_prometheus(&unlabeled).outcome,
            WindowOutcome::Baseline
        );
        assert_eq!(
            guard.observe_prometheus(labeled.as_bytes()).outcome,
            WindowOutcome::Inconsistent
        );
        assert_eq!(
            guard.observe_prometheus(labeled.as_bytes()).outcome,
            WindowOutcome::Baseline
        );
    }

    #[test]
    fn parser_rejects_missing_partial_nonfinite_and_unexpected_positions() {
        assert_eq!(
            parse_prometheus(b"other 1\n", 5),
            Err(ParseFailure::Missing)
        );
        assert_eq!(
            parse_prometheus(format!("{DRAFT_STEPS} 1\n").as_bytes(), 5),
            Err(ParseFailure::Partial)
        );
        let nonfinite = String::from_utf8(body(1, 5, 0, &[0; 5]))
            .unwrap()
            .replace(&format!("{DRAFT_STEPS} 1"), &format!("{DRAFT_STEPS} NaN"));
        assert_eq!(
            parse_prometheus(nonfinite.as_bytes(), 5),
            Err(ParseFailure::NonFinite)
        );
        assert_eq!(
            parse_prometheus(&body(1, 5, 0, &[0; 6]), 5),
            Err(ParseFailure::UnexpectedPosition)
        );
    }

    #[test]
    fn parser_rejects_semantic_duplicate_series_regardless_of_label_order() {
        let duplicate = format!(
            "{DRAFT_STEPS}{{engine=\"0\"}} 1\n\
             {PROPOSED_TOKENS}{{engine=\"0\"}} 100\n\
             {ACCEPTED_TOKENS}{{engine=\"0\"}} 0\n\
             {ACCEPTED_PER_POSITION}{{engine=\"0\",position=\"0\"}} 0\n\
             {ACCEPTED_PER_POSITION}{{position=\"0\",engine=\"0\"}} 0\n"
        );
        assert_eq!(
            parse_prometheus(duplicate.as_bytes(), 1),
            Err(ParseFailure::Duplicate)
        );
    }

    #[test]
    fn parser_is_bounded_and_rejects_invalid_utf8() {
        assert_eq!(
            parse_prometheus(&vec![b'x'; MAX_PROMETHEUS_BYTES + 1], 5),
            Err(ParseFailure::Oversized)
        );
        assert_eq!(parse_prometheus(&[0xff], 5), Err(ParseFailure::InvalidUtf8));
        assert_eq!(
            parse_prometheus(&body(1, 5, 0, &[0; 5]), 0),
            Err(ParseFailure::InvalidExpectedPositions)
        );
    }

    #[test]
    fn threshold_requires_consecutive_qualifying_active_zero_windows() {
        let mut guard = guard();
        assert_eq!(
            guard.observe(Ok(counters(0, 0, 0, &[0; 5]))).outcome,
            WindowOutcome::Baseline
        );
        for (index, proposed) in [100, 200].into_iter().enumerate() {
            let observation =
                guard.observe(Ok(counters((index + 1) as u64 * 20, proposed, 0, &[0; 5])));
            assert_eq!(observation.outcome, WindowOutcome::ZeroAcceptance);
            assert!(!observation.threshold_met);
        }
        let observation = guard.observe(Ok(counters(60, 300, 0, &[0; 5])));
        assert!(observation.newly_met);
        assert!(observation.threshold_met);
        assert!(!observation.quarantined);
        assert_eq!(
            observation.measurement,
            Some(WindowMeasurement {
                draft_steps: 20,
                proposed_tokens: 100,
                accepted_tokens: 0,
                accepted_per_position: vec![0; 5].into_boxed_slice(),
            })
        );
    }

    #[test]
    fn idle_parse_failure_clean_and_inconsistent_windows_never_trip() {
        let mut guard = guard();
        guard.observe(Ok(counters(0, 0, 0, &[0; 5])));
        guard.observe(Ok(counters(20, 100, 0, &[0; 5])));
        assert_eq!(
            guard.observe(Ok(counters(21, 150, 0, &[0; 5]))).outcome,
            WindowOutcome::Idle
        );
        guard.observe(Ok(counters(41, 250, 0, &[0; 5])));
        let failure = guard.observe(Err(ParseFailure::Partial));
        assert_eq!(failure.outcome, WindowOutcome::Parse(ParseFailure::Partial));
        assert_eq!(failure.consecutive_zero_windows, 0);
        assert_eq!(
            guard.observe(Ok(counters(61, 350, 0, &[0; 5]))).outcome,
            WindowOutcome::Baseline
        );
        let clean = guard.observe(Ok(counters(81, 450, 10, &[2; 5])));
        assert_eq!(clean.outcome, WindowOutcome::Clean);
        let inconsistent = guard.observe(Ok(counters(101, 550, 10, &[3, 2, 2, 2, 2])));
        assert_eq!(inconsistent.outcome, WindowOutcome::Inconsistent);
        assert!(!inconsistent.threshold_met);
        assert_eq!(
            guard
                .observe(Ok(counters(121, 650, 10, &[3, 2, 2, 2, 1])))
                .outcome,
            WindowOutcome::Baseline
        );
        assert_eq!(
            guard
                .observe(Ok(counters(141, 750, 10, &[3, 2, 2, 2, 1])))
                .outcome,
            WindowOutcome::ZeroAcceptance
        );
    }

    #[test]
    fn every_content_free_parse_failure_breaks_consecutiveness() {
        let failures = [
            ParseFailure::Oversized,
            ParseFailure::InvalidUtf8,
            ParseFailure::TooManyLines,
            ParseFailure::LineTooLong,
            ParseFailure::InvalidExpectedPositions,
            ParseFailure::Malformed,
            ParseFailure::Missing,
            ParseFailure::Partial,
            ParseFailure::NonFinite,
            ParseFailure::Duplicate,
            ParseFailure::MultipleSeries,
            ParseFailure::MismatchedLabels,
            ParseFailure::UnexpectedPosition,
            ParseFailure::Capacity,
            ParseFailure::Overflow,
        ];
        for failure in failures {
            let mut guard = guard();
            guard.observe(Ok(counters(0, 0, 0, &[0; 5])));
            guard.observe(Ok(counters(20, 100, 0, &[0; 5])));
            let rejected = guard.observe(Err(failure));
            assert_eq!(rejected.outcome, WindowOutcome::Parse(failure));
            assert_eq!(rejected.consecutive_zero_windows, 0);
            assert!(!rejected.threshold_met);
            assert_eq!(
                guard.observe(Ok(counters(40, 200, 0, &[0; 5]))).outcome,
                WindowOutcome::Baseline
            );
        }
    }

    #[test]
    fn payload_api_reaches_threshold_only_from_complete_windows() {
        let mut guard = guard();
        assert_eq!(
            guard.observe_prometheus(&body(0, 0, 0, &[0; 5])).outcome,
            WindowOutcome::Baseline
        );
        for window in 1..=3 {
            let observation =
                guard.observe_prometheus(&body(window * 20, window * 100, 0, &[0; 5]));
            assert_eq!(observation.outcome, WindowOutcome::ZeroAcceptance);
        }
        assert!(guard.threshold_met);
    }

    #[test]
    fn counter_reset_never_trips_and_rebaselines() {
        let mut guard = guard();
        guard.observe(Ok(counters(100, 500, 25, &[5; 5])));
        let reset = guard.observe(Ok(counters(1, 5, 0, &[0; 5])));
        assert_eq!(reset.outcome, WindowOutcome::CounterReset);
        assert!(!reset.threshold_met);
        assert_eq!(
            guard.observe(Ok(counters(21, 105, 0, &[0; 5]))).outcome,
            WindowOutcome::ZeroAcceptance
        );
    }

    #[test]
    fn observe_only_threshold_can_return_to_clean_without_quarantine() {
        let mut guard = guard();
        guard.observe(Ok(counters(0, 0, 0, &[0; 5])));
        for window in 1..=3 {
            guard.observe(Ok(counters(window * 20, window * 100, 0, &[0; 5])));
        }
        assert!(!guard.quarantined());
        let clean = guard.observe(Ok(counters(80, 400, 10, &[2; 5])));
        assert_eq!(clean.outcome, WindowOutcome::Clean);
        assert!(!clean.threshold_met);
    }

    #[test]
    fn quarantine_is_sticky_until_changed_attested_incarnation() {
        let mut guard = guard();
        assert_eq!(
            guard.attest_incarnation(incarnation(1)),
            IncarnationOutcome::Established
        );
        guard.observe(Ok(counters(0, 0, 0, &[0; 5])));
        for window in 1..=3 {
            guard.observe(Ok(counters(window * 20, window * 100, 0, &[0; 5])));
        }
        assert_eq!(guard.quarantine(), QuarantineOutcome::Entered);
        assert!(guard.quarantined());
        assert_eq!(
            guard.attest_incarnation(incarnation(1)),
            IncarnationOutcome::Unchanged
        );
        assert!(guard.quarantined());
        guard.observe(Ok(counters(80, 400, 20, &[4; 5])));
        assert!(guard.quarantined());
        assert_eq!(
            guard.attest_incarnation(incarnation(2)),
            IncarnationOutcome::Rearmed
        );
        assert!(!guard.quarantined());
        assert_eq!(
            guard.observe(Ok(counters(0, 0, 0, &[0; 5]))).outcome,
            WindowOutcome::Baseline
        );
    }

    #[test]
    fn quarantine_requires_threshold_and_existing_attestation() {
        let mut guard = guard();
        assert_eq!(guard.quarantine(), QuarantineOutcome::ThresholdNotMet);
        guard.observe(Ok(counters(0, 0, 0, &[0; 5])));
        for window in 1..=3 {
            guard.observe(Ok(counters(window * 20, window * 100, 0, &[0; 5])));
        }
        assert_eq!(
            guard.quarantine(),
            QuarantineOutcome::IncarnationUnavailable
        );
    }

    #[test]
    fn attested_change_before_quarantine_resets_cross_generation_baseline() {
        let mut guard = guard();
        guard.attest_incarnation(incarnation(1));
        guard.observe(Ok(counters(100, 500, 20, &[4; 5])));
        assert_eq!(
            guard.attest_incarnation(incarnation(2)),
            IncarnationOutcome::Changed
        );
        assert_eq!(
            guard.observe(Ok(counters(1, 5, 0, &[0; 5]))).outcome,
            WindowOutcome::Baseline
        );
    }

    #[test]
    fn configuration_rejects_single_window_or_zero_work_trip() {
        assert!(matches!(
            DsparkGuard::new(0, 3, 100),
            Err(GuardConfigError::ExpectedPositions)
        ));
        assert!(matches!(
            DsparkGuard::new(5, 1, 100),
            Err(GuardConfigError::ConsecutiveWindows)
        ));
        assert!(matches!(
            DsparkGuard::new(5, 3, 0),
            Err(GuardConfigError::MinimumProposedTokens)
        ));
    }

    #[test]
    fn labels_and_outcomes_are_fixed_and_commitment_debug_is_redacted() {
        assert_eq!(ParseFailure::Duplicate.label(), "duplicate");
        assert_eq!(ParseFailure::MultipleSeries.label(), "multiple_series");
        assert_eq!(ParseFailure::MismatchedLabels.label(), "mismatched_labels");
        assert_eq!(WindowOutcome::ZeroAcceptance.label(), "zero_acceptance");
        assert_eq!(format!("{:?}", incarnation(7)), "<redacted>");
        assert!(!format!("{:?}", incarnation(7)).contains('7'));
    }
}
