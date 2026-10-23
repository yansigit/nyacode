//! Restricted native Sentinel runtime. No JavaScript, IO, or host reflection.
//!
//! Wire primitives informed by aurora-develop/aurora (MIT), pinned in NOTICE.
//! Compatibility remains unverified until a completed native turn is retrieved.

mod layout;
mod pattern;
mod profile;
mod proof;
mod storage;
mod style;
mod vm;

pub(crate) use profile::Bootstrap;
pub(crate) use profile::Profile;
pub(crate) use proof::solve_proof;
pub(crate) use vm::Observer;
pub(crate) use vm::solve;

use serde::Serialize;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Structural failure only. Never contains instruction operands or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SentinelFailure {
    InputLimit,
    AllocationLimit,
    InstructionLimit,
    NestingLimit,
    Deadline,
    Cancelled,
    MalformedProgram,
    InvalidProgramEncoding,
    InvalidProgramJson,
    InvalidProgramShape,
    InvalidRegister,
    InvalidArray,
    InvalidQueue,
    InvalidArguments,
    InvalidJson,
    UnknownInstruction,
    UnsupportedEnvironment,
    UnsupportedScriptPattern,
    ScriptPatternLimit,
    UnsupportedScriptRegexSyntax,
    UnsupportedScriptRegexGroup,
    UnsupportedScriptRegexEscape,
    MalformedScriptRegex,
    EmptyScriptMatch,
    UnsupportedScriptMetadata,
    RequiredScriptMatchUnavailable,
    UnsupportedNumericCoercion,
    UnsupportedStringCoercion,
    UnsupportedCallbackSource,
    UnsupportedInterpreterSource,
    UnsupportedSerialization,
    UnsupportedPropertyTarget,
    RequiredNullProperty,
    RequiredUndefinedProperty,
    UnsupportedNonCallableBinding,
    UnsupportedFunctionProperty,
    UnsupportedPrimitiveProperty,
    UnsupportedHostEnumeration,
    UnsupportedPrototype,
    UnsupportedObjectConversion,
    UnsupportedNullObjectCoercion,
    UnsupportedObjectPrototype,
    UnsupportedArrayPrototype,
    UnsupportedStringPrototype,
    UnsupportedPrototypeProperty {
        value: &'static str,
        property: &'static str,
    },
    UnsupportedCreatePrototype,
    UnsupportedMutation,
    UnsupportedHostMutation,
    UnsupportedArrayMutation,
    UnsupportedTextMutation,
    UnsupportedElementMutation,
    UnsupportedDomMutation {
        element: &'static str,
        property: &'static str,
    },
    UnsupportedHtmlMutation,
    UnsupportedRenderedTextMutation,
    UnsupportedCallableMutation,
    UnsupportedWindowProperty,
    UnsupportedWindowBinding {
        property: &'static str,
    },
    UnsupportedClockApi,
    UnsupportedArrayApi,
    UnsupportedStringApi,
    UnsupportedNumberApi,
    UnsupportedFunctionApi,
    UnsupportedPromiseApi,
    UnsupportedTimers,
    UnsupportedEventTarget {
        reason: &'static str,
    },
    UnsupportedScreenApi,
    UnsupportedCryptoApi,
    UnsupportedBrowserGlobals,
    UnsupportedChromeRuntime,
    UnsupportedHistoryApi,
    UnsupportedApplicationState,
    UnsupportedRouterContext,
    RequiredRouterContextUnavailable,
    RequiredRouterContextProperty,
    RequiredRouterContextCall,
    RequiredRouterContextSerialization,
    RequiredRouterContextCoercion,
    /// Internal propagation marker; the public solver maps uncaught guest
    /// exceptions back to their structural failure, never to an error token.
    GuestException,
    UnsupportedRouterAssignment,
    InvalidRouterMetadata,
    RouterMetadataLimit,
    UnsupportedRouterMetadataShape,
    UnsupportedRouterField,
    UnsupportedRouterSessionState,
    UnsupportedRouterManifest,
    UnsupportedRouterVersion,
    UnsupportedRouterModules,
    UnsupportedRouterInstance,
    UnsupportedTelemetryState,
    UnsupportedSentinelSdkState,
    UnsupportedReflectionApi,
    UnsupportedNavigatorProperty,
    UnsupportedClientHints,
    UnsupportedStorageEstimate,
    UnsupportedStorageApi,
    UnsupportedPluginEnumeration,
    UnsupportedMemoryInfo,
    UnsupportedClipboard,
    UnsupportedPermissions,
    UnsupportedNetworkInformation,
    UnsupportedAutomationState,
    UnsupportedCookieState,
    UnsupportedBuiltinProperty,
    UnsupportedBuiltinBinding {
        object: &'static str,
        property: &'static str,
    },
    UnsupportedProfileProperty,
    UnsupportedDom,
    UnsupportedDomBody,
    UnsupportedDomBodyProperty {
        property: &'static str,
    },
    UnsupportedDomTreeMutation,
    UnsupportedDomQuery,
    UnsupportedDomCreation,
    UnsupportedFrameEnvironment,
    UnsupportedScriptExecution,
    UnsupportedDomElement,
    UnsupportedDomStyling,
    UnsupportedStyleProperty,
    UnsupportedStyleSerialization,
    UnsupportedStyleMutation,
    UnsupportedStyleValue,
    UnsupportedCssValue {
        property: &'static str,
        syntax: &'static str,
    },
    UnsupportedCssTextMutation,
    UnsupportedDomCookies,
    UnsupportedDomAttribute,
    UnsupportedDomLayout,
    UnsupportedNativeLayout {
        reason: &'static str,
    },
    UnsupportedDomMeasurement {
        element: &'static str,
        property: &'static str,
        connected: bool,
    },
    UnsupportedCanvas,
    UnsupportedCanvasContext,
    UnsupportedCanvasSerialization,
    UnsupportedStorage,
    UnsupportedGeolocation,
    UnsupportedString,
    Rejected,
    MissingResult,
    InvalidProofChallenge,
    ProofExhausted,
    BootstrapMetadataMissing,
    InteractiveChallenge,
    UnverifiedObserver,
}

pub(crate) type Result<T> = std::result::Result<T, SentinelFailure>;

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub input_bytes: usize,
    pub allocation_bytes: usize,
    pub instructions: usize,
    pub nesting: usize,
    pub elapsed: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            input_bytes: 512 * 1024,
            allocation_bytes: 8 * 1024 * 1024,
            instructions: 50_000,
            nesting: 32,
            elapsed: Duration::from_secs(2),
        }
    }
}

pub(crate) struct Budget {
    limits: Limits,
    started: Instant,
    elapsed_before_pause: Duration,
    paused: bool,
    cancellation: CancellationToken,
    allocated: usize,
    steps: usize,
}

impl Budget {
    fn new(limits: Limits, cancellation: CancellationToken) -> Self {
        Self {
            limits,
            started: Instant::now(),
            elapsed_before_pause: Duration::ZERO,
            paused: false,
            cancellation,
            allocated: 0,
            steps: 0,
        }
    }

    fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(SentinelFailure::Cancelled);
        }
        let elapsed = self.elapsed_before_pause
            + if self.paused {
                Duration::ZERO
            } else {
                self.started.elapsed()
            };
        if elapsed >= self.limits.elapsed {
            return Err(SentinelFailure::Deadline);
        }
        Ok(())
    }

    /// The observer retains its VM across an HTTP request. Bound cumulative
    /// interpreter execution, excluding time when no guest code can run.
    fn pause(&mut self) -> Result<()> {
        self.check()?;
        if !self.paused {
            self.elapsed_before_pause += self.started.elapsed();
            self.paused = true;
        }
        self.check()
    }

    fn resume(&mut self) -> Result<()> {
        if self.paused {
            self.started = Instant::now();
            self.paused = false;
        }
        self.check()
    }

    fn allocate(&mut self, bytes: usize) -> Result<()> {
        self.check()?;
        self.allocated = self
            .allocated
            .checked_add(bytes)
            .ok_or(SentinelFailure::AllocationLimit)?;
        if self.allocated > self.limits.allocation_bytes {
            return Err(SentinelFailure::AllocationLimit);
        }
        Ok(())
    }

    fn step(&mut self, depth: usize) -> Result<()> {
        self.work(1, depth)
    }

    fn work(&mut self, steps: usize, depth: usize) -> Result<()> {
        self.check()?;
        if depth > self.limits.nesting {
            return Err(SentinelFailure::NestingLimit);
        }
        self.steps = self
            .steps
            .checked_add(steps)
            .ok_or(SentinelFailure::InstructionLimit)?;
        if self.steps > self.limits.instructions {
            return Err(SentinelFailure::InstructionLimit);
        }
        Ok(())
    }
}
