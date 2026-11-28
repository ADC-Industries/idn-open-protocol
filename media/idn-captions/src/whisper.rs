//! whisper itself: the model, its device and one pass, the same whether it runs in this
//! process ([`Local`]) or in a worker process ([`crate::worker`]), which runs a `Local` of
//! its own. What the engine makes of a pass (the filters, the lines) stays with the
//! engine.

use crate::{GpuDevice, STOPPED, device, device_name};
use std::path::Path;
use std::sync::Arc;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

/// One pass's settings, as plain data (they cross to the worker).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PassParams {
    /// CPU threads (0: all but two).
    pub threads: usize,
    /// `en`, `es`…
    pub lang: String,
    /// whisper's prompt: the words to spell right and the line before, a space between
    /// (empty: none).
    pub prompt: String,
    /// At most this many tokens (measuring), else whisper's own limit.
    pub max_tokens: Option<i32>,
    /// How much hotter each try after a failed one (measuring: 0, no second try), else
    /// whisper's own.
    pub temperature_inc: Option<f32>,
}

/// One segment of a pass: its text, how sure whisper is it isn't speech, and each of its
/// tokens' probability (the special ones, at or above end-of-text, left out).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    pub text: String,
    pub no_speech: f32,
    pub probs: Vec<f32>,
}

/// Asked during a pass whether to stop it.
pub(crate) type Abort = Arc<dyn Fn() -> bool + Send + Sync>;

/// whisper's abort callback: `data` is an [`Abort`] ([`Local::pass`]).
unsafe extern "C" fn abort_trampoline(data: *mut std::ffi::c_void) -> bool {
    // SAFETY: set by `Local::pass` to a live `&Abort` for the pass's length.
    let a = unsafe { &*(data as *const Abort) };
    a()
}

/// whisper's settings for a pass.
fn full_params(p: &PassParams) -> FullParams<'_, '_> {
    let mut f = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let threads = crate::cpu_threads(p.threads);
    f.set_n_threads(threads as i32);
    f.set_language(Some(&p.lang));
    f.set_single_segment(true);
    f.set_no_context(true);
    f.set_print_progress(false);
    f.set_print_special(false);
    f.set_print_realtime(false);
    f.set_print_timestamps(false);
    f.set_suppress_blank(true);
    f.set_suppress_nst(true);
    // (A NUL in the prompt would panic in whisper-rs: a word list typed with one.)
    let prompt = p.prompt.replace('\0', "");
    if !prompt.trim().is_empty() {
        f.set_initial_prompt(&prompt);
    }
    if let Some(n) = p.max_tokens {
        f.set_max_tokens(n);
    }
    if let Some(t) = p.temperature_inc {
        f.set_temperature_inc(t);
    }
    f
}

/// whisper in this process: the model and its state.
pub(crate) struct Local {
    ctx: WhisperContext,
    state: WhisperState,
    /// What it runs on: the GPU's name, or "CPU (N threads)".
    pub device: String,
    /// Why it isn't on the GPU asked for (an index or a name not on this machine), to log.
    pub note: Option<String>,
}

impl Local {
    /// Loads the model at `path`, on the GPU `choice` gives when `gpu` (and there is
    /// one: [`crate::choose_gpu`]), else on the CPU.
    pub fn load(path: &Path, gpu: bool, choice: &GpuDevice) -> Result<Local, String> {
        whisper_rs::install_logging_hooks();
        let (g, note) = device(gpu, choice);
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu(g.is_some());
        if let Some(g) = &g {
            ctx_params.gpu_device(g.index);
        }
        let ctx = WhisperContext::new_with_params(path, ctx_params)
            .map_err(|e| format!("the model {}: {e}", path.display()))?;
        let state = ctx.create_state().map_err(|e| e.to_string())?;
        Ok(Local {
            ctx,
            state,
            device: device_name(g.as_ref()),
            note,
        })
    }

    /// One pass over `audio` (16 kHz mono); with `abort`, ended when it says so (the
    /// error [`STOPPED`]).
    pub fn pass(
        &mut self,
        p: &PassParams,
        audio: &[f32],
        abort: Option<Abort>,
    ) -> Result<Vec<Segment>, String> {
        let mut f = full_params(p);
        if let Some(a) = &abort {
            if a() {
                return Err(STOPPED.into());
            }
            // whisper-rs 0.16.0's `set_abort_callback_safe` hands its C trampoline a
            // `Box<dyn FnMut>` but reads it as the closure's own type (its progress
            // callback gets this right): the flag read was whatever lay there. The C
            // callback is set here instead, its data `a`, which outlives the pass.
            // SAFETY: `abort_trampoline` reads `user_data` as `*const Abort`, which it is
            // (`a`, borrowed for this whole function; `state.full` returns before it ends
            // and keeps nothing of `f`). `Abort` is `Sync`, so whisper's threads may call it.
            unsafe {
                f.set_abort_callback(Some(abort_trampoline));
                f.set_abort_callback_user_data(a as *const Abort as *mut std::ffi::c_void);
            }
        }
        let r = self.state.full(f, audio);
        if abort.as_ref().is_some_and(|a| a()) {
            return Err(STOPPED.into());
        }
        r.map_err(|e| e.to_string())?;
        let eot = self.ctx.token_eot();
        Ok(self
            .state
            .as_iter()
            .filter_map(|s| {
                let text = s.to_str_lossy().ok()?.into_owned();
                let probs: Vec<f32> = (0..s.n_tokens())
                    .filter_map(|i| s.get_token(i))
                    .filter(|k| k.token_id() < eot)
                    .map(|k| k.token_probability())
                    .collect();
                Some(Segment {
                    text,
                    no_speech: s.no_speech_probability(),
                    probs,
                })
            })
            .collect())
    }
}
