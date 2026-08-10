#![allow(dead_code)]
use crate::noun::slab::NounSlab;
use blake3::{Hash, Hasher};
use byteorder::{LittleEndian, WriteBytesExt};
use nockvm::hamt::Hamt;
use nockvm::interpreter::{self, interpret, Error, Mote, NockCancelToken};
use nockvm::jets::cold::{Cold, Nounable};
use nockvm::jets::hot::{HotEntry, URBIT_HOT_STATE};
use nockvm::jets::nock::util::mook;
use nockvm::mem::NockStack;
use nockvm::mug::met3_usize;
use nockvm::noun::{Atom, Cell, DirectAtom, IndirectAtom, Noun, Slots, D, T};
use nockvm::trace::{path_to_cord, write_serf_trace_safe, TraceInfo};
use nockvm_macros::tas;

use std::any::Any;
use std::fs::File;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

use crate::kernel::checkpoint::{Checkpoint, ExportedState, JamPaths, JammedCheckpoint};
use crate::nockapp::wire::{wire_to_noun, WireRepr};
use crate::noun::slam;
use crate::utils::{create_context, current_da, NOCK_STACK_SIZE, NOCK_STACK_SIZE_HUGE};
use crate::{AtomExt, CrownError, NounExt, Result, ToBytesExt};
use bincode::config::Configuration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Duration;

pub(crate) const STATE_AXIS: u64 = 6;
const LOAD_AXIS: u64 = 4;
const PEEK_AXIS: u64 = 22;
const POKE_AXIS: u64 = 23;

const SNAPSHOT_VERSION: u32 = 0;

const SERF_FINISHED_INTERVAL: Duration = Duration::from_millis(100);
const SERF_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024; // 8MB

// Actions to request of the serf thread
pub enum SerfAction {
    // Extract this state into the serf
    LoadState {
        state: Vec<u8>,
        result: oneshot::Sender<Result<()>>,
    },
    // Make a CheckPoint
    Checkpoint {
        result: oneshot::Sender<JammedCheckpoint>,
    },
    // Get the state of the serf
    GetStateBytes {
        result: oneshot::Sender<Result<Vec<u8>>>,
    },
    // Get the state noun of the kernel as a slab
    GetKernelStateSlab {
        result: oneshot::Sender<Result<NounSlab>>,
    },
    // Get the cold state as a NounSlab
    GetColdStateSlab {
        result: oneshot::Sender<NounSlab>,
    },
    // Run a peek
    Peek {
        ovo: NounSlab,
        result: oneshot::Sender<Result<NounSlab>>,
    },
    // Run a poke
    //
    // TODO: send back the event number after each poke
    Poke {
        wire: WireRepr,
        cause: NounSlab,
        result: oneshot::Sender<Result<NounSlab>>,
    },
    // Stop the loop
    Stop,
}

pub(crate) struct SerfThread {
    handle: Option<std::thread::JoinHandle<()>>,
    action_sender: mpsc::Sender<SerfAction>,
    pub cancel_token: NockCancelToken,
    inhibit: Arc<AtomicBool>,
    /// Jam persistence buffer paths.
    pub jam_paths: Arc<JamPaths>,
    /// Buffer toggle for writing to the jam buffer.
    pub buffer_toggle: Arc<AtomicBool>,
    pub event_number: Arc<AtomicU64>,
}

impl SerfThread {
    async fn new(
        nock_stack_size: usize,
        jam_paths: Arc<JamPaths>,
        kernel_bytes: Vec<u8>,
        constant_hot_state: Vec<HotEntry>,
        trace: bool,
    ) -> Result<Self> {
        let jam_paths_cloned = jam_paths.clone();
        let (action_sender, action_receiver) = mpsc::channel(1);
        let (buffer_toggle_sender, buffer_toggle_receiver) = oneshot::channel();
        let (event_number_sender, event_number_receiver) = oneshot::channel();
        let (cancel_token_sender, cancel_token_receiver) = oneshot::channel();
        let inhibit = Arc::new(AtomicBool::new(false));
        let inhibit_clone = inhibit.clone();
        std::fs::create_dir_all(jam_paths.0.parent().unwrap_or_else(|| {
            panic!(
                "Panicked at {}:{} (git sha: {:?})",
                file!(),
                line!(),
                option_env!("GIT_SHA")
            )
        }))
        .unwrap_or_else(|err| {
            panic!(
                "Panicked with {err:?} at {}:{} (git sha: {:?})",
                file!(),
                line!(),
                option_env!("GIT_SHA")
            )
        });
        let handle = std::thread::Builder::new()
            .name("serf".to_string())
            .stack_size(SERF_THREAD_STACK_SIZE)
            .spawn(move || {
                let mut stack = NockStack::new(nock_stack_size, 0);
                let checkpoint = if jam_paths.checkpoint_exists() {
                    info!("Found existing state - restoring from checkpoint");
                    jam_paths.load_checkpoint(&mut stack).ok()
                } else {
                    info!("No existing state found - initializing fresh state");
                    None
                };
                let buffer_toggle = Arc::new(AtomicBool::new(
                    checkpoint
                        .as_ref()
                        .map_or_else(|| false, |checkpoint| !checkpoint.buff_index),
                ));
                buffer_toggle_sender
                    .send(buffer_toggle.clone())
                    .expect("Could not send buffer toggle out of serf thread");
                let serf = Serf::new(stack, checkpoint, &kernel_bytes, &constant_hot_state, trace);
                event_number_sender
                    .send(serf.event_num.clone())
                    .expect("Could not send event number out of serf thread");
                cancel_token_sender
                    .send(serf.context.cancel_token())
                    .expect("Could not send cancel token out of serf thread");
                serf_loop(serf, action_receiver, buffer_toggle, inhibit_clone);
            })?;

        let buffer_toggle = buffer_toggle_receiver.await?;
        let event_number = event_number_receiver.await?;
        let cancel_token = cancel_token_receiver.await?;
        Ok(SerfThread {
            inhibit,
            buffer_toggle,
            handle: Some(handle),
            action_sender,
            jam_paths: jam_paths_cloned,
            event_number,
            cancel_token,
        })
    }

    pub(crate) fn stop(&mut self) -> impl Future<Output = Result<()>> {
        let action_sender = self.action_sender.clone();
        let cancel_token = self.cancel_token.clone();
        let join_handle = self.handle.take().expect("Serf join handle already taken.");
        let tokio_join_handle = tokio::task::spawn_blocking(move || join_handle.join());
        self.inhibit.store(true, Ordering::SeqCst);
        async move {
            cancel_token.cancel();
            action_sender
                .send(SerfAction::Stop)
                .await
                .expect("Failed to send stop action");
            match tokio_join_handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(CrownError::Unknown(format!("Serf thread panicked: {e:?}"))),
                Err(e) => Err(CrownError::JoinError(e)),
            }
        }
    }

    pub(crate) fn join(&mut self) -> Result<(), Box<dyn Any + Send + 'static>> {
        self.handle
            .take()
            .expect("Serf thread already joined")
            .join()
    }

    pub(crate) async fn get_kernel_state_slab(&self) -> Result<NounSlab> {
        let (result, result_fut) = oneshot::channel();
        self.action_sender
            .send(SerfAction::GetKernelStateSlab { result })
            .await?;
        result_fut.await?
    }

    pub(crate) async fn get_cold_state_slab(&self) -> Result<NounSlab> {
        let (result, result_fut) = oneshot::channel();
        self.action_sender
            .send(SerfAction::GetColdStateSlab { result })
            .await?;
        Ok(result_fut.await?)
    }

    pub(crate) fn peek(&self, ovo: NounSlab) -> impl Future<Output = Result<NounSlab>> {
        let (result, result_fut) = oneshot::channel();
        let action_sender = self.action_sender.clone();
        async move {
            action_sender.send(SerfAction::Peek { ovo, result }).await?;
            result_fut.await?
        }
    }

    // We are very carefully ensuring that the future does not contain the &self reference, to allow spawning a task without lifetime issues
    pub(crate) fn poke(
        &self,
        wire: WireRepr,
        cause: NounSlab,
    ) -> impl Future<Output = Result<NounSlab>> {
        let (result, result_fut) = oneshot::channel();
        let action_sender = self.action_sender.clone();
        async move {
            action_sender
                .send(SerfAction::Poke {
                    wire,
                    cause,
                    result,
                })
                .await?;
            result_fut.await?
        }
    }

    pub(crate) fn poke_sync(&self, wire: WireRepr, cause: NounSlab) -> Result<NounSlab> {
        let (result, result_fut) = oneshot::channel();
        self.action_sender.blocking_send(SerfAction::Poke {
            wire,
            cause,
            result,
        })?;
        result_fut.blocking_recv()?
    }

    pub(crate) fn peek_sync(&self, ovo: NounSlab) -> Result<NounSlab> {
        let (result, result_fut) = oneshot::channel();
        self.action_sender
            .blocking_send(SerfAction::Peek { ovo, result })?;
        result_fut.blocking_recv()?
    }

    pub(crate) async fn load_state_from_bytes(&self, state: Vec<u8>) -> Result<()> {
        let (result, result_fut) = oneshot::channel();
        self.action_sender
            .send(SerfAction::LoadState { state, result })
            .await?;
        result_fut.await?
    }

    pub(crate) async fn create_state_bytes(&self) -> Result<Vec<u8>> {
        let (result, result_fut) = oneshot::channel();
        self.action_sender
            .send(SerfAction::GetStateBytes { result })
            .await?;
        result_fut.await?
    }

    pub(crate) fn checkpoint(&self) -> impl Future<Output = Result<JammedCheckpoint>> {
        let (result, result_fut) = oneshot::channel();
        let action_sender = self.action_sender.clone();
        async move {
            action_sender
                .send(SerfAction::Checkpoint { result })
                .await?;
            Ok(result_fut.await?)
        }
    }
}

fn load_state_from_bytes(serf: &mut Serf, state_bytes: &[u8]) -> Result<()> {
    let noun = extract_state_from_bytes(serf.stack(), state_bytes)?;
    serf.load(noun)?;
    Ok(())
}

fn serf_loop(
    mut serf: Serf,
    mut action_receiver: mpsc::Receiver<SerfAction>,
    buffer_toggle: Arc<AtomicBool>,
    inhibit: Arc<AtomicBool>,
) {
    loop {
        let Some(action) = action_receiver.blocking_recv() else {
            break;
        };
        match action {
            SerfAction::LoadState { state, result } => {
                let res = load_state_from_bytes(&mut serf, &state);
                let _ = result.send(res).map_err(|e| {
                    debug!("Could not send load state result");
                    e
                });
            }
            SerfAction::Stop => {
                break;
            }
            SerfAction::GetStateBytes { result } => {
                let state_bytes = create_state_bytes(&mut serf);
                let _ = result.send(state_bytes).map_err(|e| {
                    debug!("Could not send state bytes to dropped channel");
                    e
                });
            }
            SerfAction::GetKernelStateSlab { result } => {
                let kernel_state_noun = serf.arvo.slot(STATE_AXIS);
                let kernel_state_slab = kernel_state_noun.map_or_else(
                    |err| Err(CrownError::from(err)),
                    |noun| {
                        let mut slab = NounSlab::new();
                        slab.copy_into(noun);
                        Ok(slab)
                    },
                );
                let _ = result.send(kernel_state_slab).map_err(|e| {
                    debug!("Tried to send to dropped result channel");
                    e
                });
            }
            SerfAction::GetColdStateSlab { result } => {
                let cold_state_noun = serf.context.cold.into_noun(serf.stack());
                let cold_state_slab = {
                    let mut slab = NounSlab::new();
                    slab.copy_into(cold_state_noun);
                    slab
                };
                let _ = result.send(cold_state_slab).map_err(|e| {
                    debug!("Could not send cold state to dropped channel.");
                    e
                });
            }
            SerfAction::Checkpoint { result } => {
                let checkpoint = create_checkpoint(&mut serf, buffer_toggle.clone());
                //result.send(checkpoint).expect("Could not send checkpoint");
                if result.send(checkpoint).is_err() {
                    debug!(
                        "Checkpoint receiver dropped before receiving result - likely timed out"
                    );
                }
            }
            SerfAction::Peek { ovo, result } => {
                if inhibit.load(Ordering::SeqCst) {
                    let _ = result
                        .send(Err(CrownError::Unknown("Serf stopping".to_string())))
                        .map_err(|e| {
                            debug!("Tried to send inhibited peek state to dropped channel");
                            e
                        });
                } else {
                    let ovo_noun = ovo.copy_to_stack(serf.stack());
                    let noun_res = serf.peek(ovo_noun);
                    let noun_slab_res = noun_res.map(|noun| {
                        let mut slab = NounSlab::new();
                        slab.copy_into(noun);
                        slab
                    });
                    let _ = result.send(noun_slab_res).map_err(|e| {
                        debug!("Tried to send peek state to dropped channel");
                        e
                    });
                }
            }
            SerfAction::Poke {
                wire,
                cause,
                result,
            } => {
                if inhibit.load(Ordering::SeqCst) {
                    let _ = result
                        .send(Err(CrownError::Unknown("Serf stopping".to_string())))
                        .map_err(|e| {
                            debug!("Failed to send inihibited poke result from serf thread");
                            e
                        });
                } else {
                    let cause_noun = cause.copy_to_stack(serf.stack());
                    let noun_res = serf.poke(wire, cause_noun);
                    let noun_slab_res = noun_res.map(|noun| {
                        let mut slab = NounSlab::new();
                        slab.copy_into(noun);
                        slab
                    });
                    let _ = result.send(noun_slab_res).map_err(|e| {
                        debug!("Failed to send poke result from serf thread");
                        e
                    });
                }
            }
        }
    }
}

/// Extracts the kernel state from a jammed checkpoint or exported state
fn extract_state_from_bytes(stack: &mut NockStack, state_bytes: &[u8]) -> Result<Noun> {
    // First try to decode as JammedCheckpoint
    match extract_from_checkpoint(stack, state_bytes) {
        Ok(noun) => {
            debug!("Successfully loaded state from JammedCheckpoint format");
            Ok(noun)
        }
        Err(e1) => {
            debug!("Failed to load as JammedCheckpoint: {}", e1);

            // Then try to decode as ExportedState
            match extract_from_exported_state(stack, state_bytes) {
                Ok(noun) => {
                    debug!("Successfully loaded state from ExportedState format");
                    Ok(noun)
                }
                Err(e2) => {
                    warn!("Failed to load as ExportedState: {}", e2);
                    warn!("State bytes format is not recognized");
                    Err(CrownError::StateJamFormatError)
                }
            }
        }
    }
}

/// Extracts the kernel state from an ExportedState
fn extract_from_exported_state(stack: &mut NockStack, state_jam: &[u8]) -> Result<Noun> {
    let config = bincode::config::standard();

    // Try to decode as ExportedState
    let (exported, _) = bincode::decode_from_slice::<ExportedState, Configuration>(
        state_jam, config,
    )
    .map_err(|e| {
        debug!("Failed to decode state jam as ExportedState: {}", e);
        CrownError::StateJamFormatError
    })?;

    // Verify the magic bytes
    if exported.magic_bytes != tas!(b"EXPJAM") {
        debug!("Invalid magic bytes for ExportedState: expected EXPJAM");
        return Err(CrownError::StateJamFormatError);
    }

    // Extract the kernel state from the jammed noun
    let noun = <Noun as NounExt>::cue_bytes(stack, &exported.jam.0).map_err(|e| {
        warn!("Failed to cue bytes from exported state jam: {:?}", e);
        CrownError::StateJamFormatError
    })?;

    debug!("Successfully extracted kernel state from ExportedState");
    Ok(noun)
}

/// Extracts the kernel state from a JammedCheckpoint
fn extract_from_checkpoint(stack: &mut NockStack, state_jam: &[u8]) -> Result<Noun> {
    let config = bincode::config::standard();

    // Try to decode as JammedCheckpoint
    let (checkpoint, _) = bincode::decode_from_slice::<JammedCheckpoint, Configuration>(
        state_jam, config,
    )
    .map_err(|e| {
        debug!("Failed to decode state jam as JammedCheckpoint: {}", e);
        CrownError::StateJamFormatError
    })?;

    // Verify the magic bytes
    if checkpoint.magic_bytes != tas!(b"CHKJAM") {
        debug!("Invalid magic bytes for JammedCheckpoint: expected CHKJAM");
        return Err(CrownError::StateJamFormatError);
    }

    // Validate the checkpoint
    if !checkpoint.validate() {
        warn!("Checkpoint validation failed");
        return Err(CrownError::StateJamFormatError);
    }

    // Extract the kernel state from the jammed noun
    let cell = <Noun as NounExt>::cue_bytes(stack, &checkpoint.jam.0)
        .map_err(|e| {
            warn!("Failed to cue bytes from checkpoint jam: {:?}", e);
            CrownError::StateJamFormatError
        })?
        .as_cell()
        .map_err(|e| {
            warn!("Failed to convert noun to cell: {}", e);
            CrownError::StateJamFormatError
        })?;

    // The kernel state is the head of the cell
    debug!("Successfully extracted kernel state from JammedCheckpoint");
    Ok(cell.head())
}

/// Creates a serialized byte array from the current kernel state
fn create_state_bytes(serf: &mut Serf) -> Result<Vec<u8>> {
    let version = serf.version;
    let ker_hash = serf.ker_hash;
    let event_num = serf.event_num.load(Ordering::SeqCst);
    let ker_state = serf.arvo.slot(STATE_AXIS).unwrap_or_else(|err| {
        panic!(
            "Panicked with {err:?} at {}:{} (git sha: {:?})",
            file!(),
            line!(),
            option_env!("GIT_SHA")
        )
    });

    let exported_state = ExportedState::new(serf.stack(), version, ker_hash, event_num, &ker_state);

    let encoded = exported_state
        .encode()
        .map_err(|_| CrownError::StateJamFormatError)?;

    Ok(encoded)
}

fn create_checkpoint(serf: &mut Serf, buffer_toggle: Arc<AtomicBool>) -> JammedCheckpoint {
    let version = serf.version;
    let ker_hash = serf.ker_hash;
    let event_num = serf.event_num.load(Ordering::SeqCst);
    let ker_state = serf.arvo.slot(STATE_AXIS).unwrap_or_else(|err| {
        panic!(
            "Panicked with {err:?} at {}:{} (git sha: {:?})",
            file!(),
            line!(),
            option_env!("GIT_SHA")
        )
    });
    let cold = serf.context.cold;
    let buff_index = buffer_toggle.load(Ordering::SeqCst);
    JammedCheckpoint::new(
        serf.stack(),
        version,
        buff_index,
        ker_hash,
        event_num,
        &cold,
        &ker_state,
    )
}

/// Represents a Sword kernel, containing a Serf and snapshot location.
pub struct Kernel {
    /// The Serf managing the interface to the Sword.
    pub(crate) serf: SerfThread,
    /// Directory path for storing pma snapshots.
    pma_dir: Arc<PathBuf>,
}

impl Kernel {
    /// Loads a kernel with a custom hot state.
    ///
    /// # Arguments
    ///
    /// * `snap_dir` - Directory for storing snapshots.
    /// * `kernel` - Byte slice containing the kernel as a jammed noun.
    /// * `hot_state` - Custom hot state entries.
    /// * `trace` - Whether to enable tracing.
    ///
    /// # Returns
    ///
    /// A new `Kernel` instance.
    pub async fn load_with_hot_state(
        pma_dir: PathBuf,
        jam_paths: JamPaths,
        kernel: &[u8],
        hot_state: &[HotEntry],
        trace: bool,
    ) -> Result<Self> {
        let jam_paths_arc = Arc::new(jam_paths);
        let kernel_vec = Vec::from(kernel);
        let hot_state_vec = Vec::from(hot_state);
        let pma_dir_arc = Arc::new(pma_dir);
        let serf = SerfThread::new(
            NOCK_STACK_SIZE, jam_paths_arc, kernel_vec, hot_state_vec, trace,
        )
        .await?;
        Ok(Self {
            serf,
            pma_dir: pma_dir_arc,
        })
    }

    pub async fn load_with_hot_state_huge(
        pma_dir: PathBuf,
        jam_paths: JamPaths,
        kernel: &[u8],
        hot_state: &[HotEntry],
        trace: bool,
    ) -> Result<Self> {
        let jam_paths_arc = Arc::new(jam_paths);
        let kernel_vec = Vec::from(kernel);
        let hot_state_vec = Vec::from(hot_state);
        let pma_dir_arc = Arc::new(pma_dir);
        let serf = SerfThread::new(
            NOCK_STACK_SIZE_HUGE, jam_paths_arc, kernel_vec, hot_state_vec, trace,
        )
        .await?;
        Ok(Self {
            serf,
            pma_dir: pma_dir_arc,
        })
    }

    /// Loads a kernel with default hot state.
    ///
    /// # Arguments
    ///
    /// * `snap_dir` - Directory for storing snapshots.
    /// * `kernel` - Byte slice containing the kernel code.
    /// * `trace` - Whether to enable tracing.
    ///
    /// # Returns
    ///
    /// A new `Kernel` instance.
    pub async fn load(
        pma_dir: PathBuf,
        jam_paths: JamPaths,
        kernel: &[u8],
        trace: bool,
    ) -> Result<Self> {
        Self::load_with_hot_state(pma_dir, jam_paths, kernel, &Vec::new(), trace).await
    }

    /// Loads a kernel with state from jammed bytes
    pub async fn load_with_kernel_state(
        pma_dir: PathBuf,
        jam_paths: JamPaths,
        kernel_jam: &[u8],
        state_bytes: &[u8],
        hot_state: &[HotEntry],
        trace: bool,
    ) -> Result<Self> {
        let kernel =
            Self::load_with_hot_state(pma_dir, jam_paths, kernel_jam, hot_state, trace).await?;

        match kernel
            .serf
            .load_state_from_bytes(Vec::from(state_bytes))
            .await
        {
            Ok(_) => {
                debug!("Successfully loaded state from bytes");
                Ok(kernel)
            }
            Err(e) => {
                error!("Failed to load state from state bytes: {}", e);
                error!("The state bytes format is not recognized. It should be either a JammedCheckpoint or an ExportedState.");
                Err(e)
            }
        }
    }

    /// Produces a checkpoint of the kernel state.
    pub fn checkpoint(&self) -> impl Future<Output = Result<JammedCheckpoint>> {
        self.serf.checkpoint()
    }

    // We are very carefully ensuring the future does not contain the "self" reference to ensure no lifetime issues when spawning tasks
    pub fn poke(&self, wire: WireRepr, cause: NounSlab) -> impl Future<Output = Result<NounSlab>> {
        self.serf.poke(wire, cause)
    }

    pub fn poke_sync(&self, wire: WireRepr, cause: NounSlab) -> Result<NounSlab> {
        self.serf.poke_sync(wire, cause)
    }

    pub fn peek_sync(&self, ovo: NounSlab) -> Result<NounSlab> {
        self.serf.peek_sync(ovo)
    }

    // We are very carefully ensuring the future does not contain the "self" reference to ensure no lifetime issues when spawning tasks
    #[tracing::instrument(name = "crown::Kernel::peek", skip_all)]
    pub(crate) fn peek(&self, ovo: NounSlab) -> impl Future<Output = Result<NounSlab>> {
        self.serf.peek(ovo)
    }

    pub async fn create_state_bytes(&self) -> Result<Vec<u8>> {
        self.serf.create_state_bytes().await
    }
}

/// Represents the Serf, which maintains context and provides an interface to
/// the Sword.
pub struct Serf {
    /// Version number of snapshot
    pub version: u32,
    /// Hash of boot kernel
    pub ker_hash: Hash,
    /// The current Arvo state.
    pub arvo: Noun,
    /// The interpreter context.
    pub context: interpreter::Context,
    /// Cancellation
    pub cancel_token: NockCancelToken,
    /// The current event number.
    pub event_num: Arc<AtomicU64>,
}

impl Serf {
    /// Creates a new Serf instance.
    ///
    /// # Arguments
    ///
    /// * `stack` - The Nock stack.
    /// * `checkpoint` - Optional checkpoint to restore from.
    /// * `kernel_bytes` - Byte slice containing the kernel code.
    /// * `constant_hot_state` - Custom hot state entries.
    /// * `trace` - Bool indicating whether to enable nockvm tracing.
    ///
    /// # Returns
    ///
    /// A new `Serf` instance.
    fn new(
        mut stack: NockStack,
        checkpoint: Option<Checkpoint>,
        kernel_bytes: &[u8],
        constant_hot_state: &[HotEntry],
        trace: bool,
    ) -> Self {
        let hot_state = [URBIT_HOT_STATE, constant_hot_state].concat();

        let (cold, event_num_raw) = checkpoint.as_ref().map_or_else(
            || (Cold::new(&mut stack), 0),
            |snapshot| (snapshot.cold, snapshot.event_num),
        );

        let event_num = Arc::new(AtomicU64::new(event_num_raw));

        let trace_info = if trace {
            let file = File::create("trace.json").expect("Cannot create trace file trace.json");
            let pid = std::process::id();
            let process_start = std::time::Instant::now();
            Some(TraceInfo {
                file,
                pid,
                process_start,
            })
        } else {
            None
        };

        let mut context = create_context(stack, &hot_state, cold, trace_info);
        let cancel_token = context.cancel_token();

        let version = checkpoint
            .as_ref()
            .map_or_else(|| SNAPSHOT_VERSION, |snapshot| snapshot.version);

        let mut arvo = {
            let kernel_trap = Noun::cue_bytes_slice(&mut context.stack, kernel_bytes)
                .expect("invalid kernel jam");
            let fol = T(&mut context.stack, &[D(9), D(2), D(0), D(1)]);

            if context.trace_info.is_some() {
                let start = Instant::now();
                let arvo = interpret(&mut context, kernel_trap, fol).unwrap_or_else(|err| {
                    panic!(
                        "Panicked with {err:?} at {}:{} (git sha: {:?})",
                        file!(),
                        line!(),
                        option_env!("GIT_SHA")
                    )
                });
                write_serf_trace_safe(&mut context, "boot", start);
                arvo
            } else {
                interpret(&mut context, kernel_trap, fol).unwrap_or_else(|err| {
                    panic!(
                        "Panicked with {err:?} at {}:{} (git sha: {:?})",
                        file!(),
                        line!(),
                        option_env!("GIT_SHA")
                    )
                })
            }
        };

        let mut hasher = Hasher::new();
        hasher.update(kernel_bytes);
        let ker_hash = hasher.finalize();

        let mut serf = Self {
            version,
            ker_hash,
            arvo,
            context,
            event_num,
            cancel_token,
        };

        if let Some(checkpoint) = checkpoint {
            if ker_hash != checkpoint.ker_hash {
                info!(
                    "Kernel hash changed: {:?} -> {:?}",
                    checkpoint.ker_hash, ker_hash
                );
            }
            arvo = serf.load(checkpoint.ker_state).expect("serf: load failed");
        }

        unsafe {
            serf.event_update(event_num_raw, arvo);
            serf.preserve_event_update_leftovers();
        }
        serf
    }

    /// Performs a peek operation on the Arvo state.
    ///
    /// # Arguments
    ///
    /// * `ovo` - The peek request noun.
    ///
    /// # Returns
    ///
    /// Result containing the peeked data or an error.
    #[tracing::instrument(skip_all)]
    pub fn peek(&mut self, ovo: Noun) -> Result<Noun> {
        if self.context.trace_info.is_some() {
            let trace_name = "peek";
            let start = Instant::now();
            let slam_res = self.slam(PEEK_AXIS, ovo);
            write_serf_trace_safe(&mut self.context, trace_name, start);

            slam_res
        } else {
            self.slam(PEEK_AXIS, ovo)
        }
    }

    /// Generates a goof (error) noun.
    ///
    /// # Arguments
    ///
    /// * `mote` - The error mote.
    /// * `traces` - Trace information.
    ///
    /// # Returns
    ///
    /// A noun representing the error.
    pub fn goof(&mut self, mote: Mote, traces: Noun) -> Noun {
        let tone = Cell::new(&mut self.context.stack, D(2), traces);
        let tang = mook(&mut self.context, tone, false)
            .expect("serf: goof: +mook crashed on bail")
            .tail();
        T(&mut self.context.stack, &[D(mote as u64), tang])
    }

    /// Performs a load operation on the Arvo state.
    ///
    /// # Arguments
    ///
    /// * `old` - The state to load.
    ///
    /// # Returns
    ///
    /// Result containing the loaded kernel or an error.
    pub fn load(&mut self, old: Noun) -> Result<Noun> {
        match self.soft(old, LOAD_AXIS, Some("load".to_string())) {
            Ok(res) => Ok(res),
            Err(goof) => {
                self.print_goof(goof);
                Err(CrownError::SerfLoadError)
            }
        }
    }

    pub fn print_goof(&mut self, goof: Noun) {
        let tang = goof
            .as_cell()
            .expect("print goof: expected goof to be a cell")
            .tail();
        tang.list_iter().for_each(|tank: Noun| {
            //  TODO: Slogger should be emitting Results in case of failure
            self.context.slogger.slog(&mut self.context.stack, 1, tank);
        });
    }

    /// Performs a poke operation on the Arvo state.
    ///
    /// # Arguments
    ///
    /// * `job` - The poke job noun.
    ///
    /// # Returns
    ///
    /// Result containing the poke response or an error.
    #[tracing::instrument(level = "info", skip_all)]
    pub fn do_poke(&mut self, job: Noun) -> Result<Noun> {
        // Jojo scry roof: make a Jock kernel's peekContext (Nock 12)
        // RESOLVE instead of BAIL_EXIT ("no scry handler").  The
        // interpreter slams scry_stack's head gate with [reff path]
        // and double-unwraps [~ [~ v]] -> v.  We install a one-handler
        // stack [gate 0] whose gate is a small string-keyed NAMESPACE
        // that dispatches on the scried path (axis 13 of the scry core
        // [battery [reff path] context]); a Jock String is a @t cord,
        // so the path compares by atom equality:
        //     peekContext("answer") -> 42
        //     peekContext("pi")     -> 314
        //     peekContext("hoon")   -> 1337
        //     anything else         -> ~ (block; a real .^ crashes,
        //                                  the Arvo "come back later")
        // This is the "NockApp answers nothing until you register a
        // roof" side; the Gall/Arvo side wires the gate to .^ instead.
        let roof = {
            let stack = &mut self.context.stack;
            let path = T(stack, &[D(0), D(13)]); // [0 13] -> scried path
            let block = T(stack, &[D(1), D(0)]); // [1 0] -> ~ (block)
            // one arm per entry: [6 [5 path [1 cord]] [1 0 0 val] else]
            // where [1 0 0 val] is [~ [~ val]] and 6 falls through on miss.
            let lit_hoon = T(stack, &[D(1), D(1852796776)]); // "hoon"
            let eq_hoon = T(stack, &[D(5), path, lit_hoon]);
            let ans_hoon = T(stack, &[D(1), D(0), D(0), D(1337)]);
            let c_hoon = T(stack, &[D(6), eq_hoon, ans_hoon, block]);
            let lit_pi = T(stack, &[D(1), D(26992)]); // "pi"
            let eq_pi = T(stack, &[D(5), path, lit_pi]);
            let ans_pi = T(stack, &[D(1), D(0), D(0), D(314)]);
            let c_pi = T(stack, &[D(6), eq_pi, ans_pi, c_hoon]);
            let lit_ans = T(stack, &[D(1), D(125780121316961)]); // "answer"
            let eq_ans = T(stack, &[D(5), path, lit_ans]);
            let ans_ans = T(stack, &[D(1), D(0), D(0), D(42)]);
            let battery = T(stack, &[D(6), eq_ans, ans_ans, c_pi]);
            let roof_gate = T(stack, &[battery, D(0), D(0)]); // [batt 0 0]
            T(stack, &[roof_gate, D(0)]) // scry_stack = [gate 0]
        };
        self.context.scry_stack = roof;
        match self.soft(job, POKE_AXIS, Some("poke".to_string())) {
            Ok(res) => {
                let cell = res.as_cell().expect("serf: poke: +slam returned atom");
                let mut fec = cell.head();
                let eve = self.event_num.load(Ordering::SeqCst);

                unsafe {
                    self.event_update(eve + 1, cell.tail());
                    self.stack().preserve(&mut fec);
                    self.preserve_event_update_leftovers();
                }
                Ok(fec)
            }
            Err(goof) => self.poke_swap(job, goof),
        }
    }

    /// Slams (applies) a gate at a specific axis of Arvo.
    ///
    /// # Arguments
    ///
    /// * `axis` - The axis to slam.
    /// * `ovo` - The sample noun.
    ///
    /// # Returns
    ///
    /// Result containing the slammed result or an error.
    pub fn slam(&mut self, axis: u64, ovo: Noun) -> Result<Noun> {
        let arvo = self.arvo;
        slam(&mut self.context, arvo, axis, ovo)
    }

    /// Performs a "soft" computation, handling errors gracefully.
    ///
    /// # Arguments
    ///
    /// * `ovo` - The input noun.
    /// * `axis` - The axis to slam.
    /// * `trace_name` - Optional name for tracing.
    ///
    /// # Returns
    ///
    /// Result containing the computed noun or an error noun.
    fn soft(&mut self, ovo: Noun, axis: u64, trace_name: Option<String>) -> Result<Noun, Noun> {
        let slam_res = if self.context.trace_info.is_some() {
            let start = Instant::now();
            let slam_res = self.slam(axis, ovo);
            write_serf_trace_safe(
                &mut self.context,
                trace_name.as_ref().unwrap_or_else(|| {
                    panic!(
                        "Panicked at {}:{} (git sha: {:?})",
                        file!(),
                        line!(),
                        option_env!("GIT_SHA")
                    )
                }),
                start,
            );

            slam_res
        } else {
            self.slam(axis, ovo)
        };

        match slam_res {
            Ok(res) => Ok(res),
            Err(error) => match error {
                CrownError::InterpreterError(e) => {
                    let (mote, traces) = match e.0 {
                        Error::Deterministic(mote, traces)
                        | Error::NonDeterministic(mote, traces) => (mote, traces),
                        Error::ScryBlocked(_) | Error::ScryCrashed(_) => {
                            panic!("serf: soft: .^ invalid outside of virtual Nock")
                        }
                    };

                    Err(self.goof(mote, traces))
                }
                _ => Err(D(0)),
            },
        }
    }

    /// Plays a list of events.
    ///
    /// # Arguments
    ///
    /// * `lit` - The list of events to play.
    ///
    /// # Returns
    ///
    /// Result containing the final Arvo state or an error.
    fn play_list(&mut self, mut lit: Noun) -> Result<Noun> {
        let mut eve = self.event_num.load(Ordering::SeqCst);
        while let Ok(cell) = lit.as_cell() {
            let ovo = cell.head();
            lit = cell.tail();
            let trace_name = if self.context.trace_info.is_some() {
                Some(format!("play [{}]", eve))
            } else {
                None
            };

            match self.soft(ovo, POKE_AXIS, trace_name) {
                Ok(res) => {
                    let arvo = res.as_cell()?.tail();
                    eve += 1;

                    unsafe {
                        self.event_update(eve, arvo);
                        self.context.stack.preserve(&mut lit);
                        self.preserve_event_update_leftovers();
                    }
                }
                Err(goof) => {
                    return Err(CrownError::KernelError(Some(goof)));
                }
            }
        }
        Ok(self.arvo)
    }

    /// Handles a poke error by swapping in a new event.
    ///
    /// # Arguments
    ///
    /// * `job` - The original poke job.
    /// * `goof` - The error noun.
    ///
    /// # Returns
    ///
    /// Result containing the new event or an error.
    fn poke_swap(&mut self, job: Noun, goof: Noun) -> Result<Noun> {
        let stack = &mut self.context.stack;
        self.context.cache = Hamt::<Noun>::new(stack);
        let job_cell = job.as_cell().expect("serf: poke: job not a cell");
        // job data is job without event_num
        let job_data = job_cell
            .tail()
            .as_cell()
            .expect("serf: poke: data not a cell");
        //  job input is job without event_num or wire
        let job_input = job_data.tail();
        let wire = T(stack, &[D(0), D(tas!(b"arvo")), D(0)]);
        let crud = DirectAtom::new_panic(tas!(b"crud"));
        let event_num = D(self.event_num.load(Ordering::SeqCst) + 1);

        let mut ovo = T(stack, &[event_num, wire, goof, job_input]);
        let trace_name = if self.context.trace_info.is_some() {
            Some(Self::poke_trace_name(
                &mut self.context.stack,
                wire,
                crud.as_atom(),
            ))
        } else {
            None
        };

        match self.soft(ovo, POKE_AXIS, trace_name) {
            Ok(res) => {
                let cell = res.as_cell().expect("serf: poke: crud +slam returned atom");
                let mut fec = cell.head();
                let eve = self.event_num.load(Ordering::SeqCst);

                unsafe {
                    self.event_update(eve + 1, cell.tail());
                    self.context.stack.preserve(&mut ovo);
                    self.context.stack.preserve(&mut fec);
                    self.preserve_event_update_leftovers();
                }
                Ok(self.poke_bail(eve, eve, ovo, fec))
            }
            Err(goof_crud) => {
                let stack = &mut self.context.stack;
                let lud = T(stack, &[goof_crud, goof, D(0)]);
                Ok(self.poke_bail_noun(lud))
            }
        }
    }

    /// Generates a trace name for a poke operation.
    ///
    /// # Arguments
    ///
    /// * `stack` - The Nock stack.
    /// * `wire` - The wire noun.
    /// * `vent` - The vent atom.
    ///
    /// # Returns
    ///
    /// A string representing the trace name.
    fn poke_trace_name(stack: &mut NockStack, wire: Noun, vent: Atom) -> String {
        let wpc = path_to_cord(stack, wire);
        let wpc_len = met3_usize(wpc);
        let wpc_bytes = &wpc.as_ne_bytes()[0..wpc_len];
        let wpc_str = match std::str::from_utf8(wpc_bytes) {
            Ok(valid) => valid,
            Err(error) => {
                let (valid, _) = wpc_bytes.split_at(error.valid_up_to());
                unsafe { std::str::from_utf8_unchecked(valid) }
            }
        };

        let vc_len = met3_usize(vent);
        let vc_bytes = &vent.as_ne_bytes()[0..vc_len];
        let vc_str = match std::str::from_utf8(vc_bytes) {
            Ok(valid) => valid,
            Err(error) => {
                let (valid, _) = vc_bytes.split_at(error.valid_up_to());
                unsafe { std::str::from_utf8_unchecked(valid) }
            }
        };

        format!("poke [{} {}]", wpc_str, vc_str)
    }

    /// Performs a poke operation with a given cause.
    ///
    /// # Arguments
    ///
    /// * `wire` - The wire noun.
    /// * `cause` - The cause noun.
    ///
    /// # Returns
    ///
    /// Result containing the poke response or an error.
    #[tracing::instrument(level = "info", skip_all, fields(
        src = wire.source
    ))]
    pub fn poke(&mut self, wire: WireRepr, cause: Noun) -> Result<Noun> {
        let random_bytes = rand::random::<u64>();
        let bytes = random_bytes.as_bytes()?;
        let eny: Atom = Atom::from_bytes(&mut self.context.stack, &bytes);
        let our = <nockvm::noun::Atom as AtomExt>::from_value(&mut self.context.stack, 0)?; // Using 0 as default value
        let now: Atom = unsafe {
            let mut t_vec: Vec<u8> = vec![];
            t_vec.write_u128::<LittleEndian>(current_da().0)?;
            IndirectAtom::new_raw_bytes(&mut self.context.stack, 16, t_vec.as_slice().as_ptr())
                .normalize_as_atom()
        };

        let event_num = D(self.event_num.load(Ordering::SeqCst) + 1);
        let base_wire_noun = wire_to_noun(&mut self.context.stack, &wire);
        let wire = T(&mut self.context.stack, &[D(tas!(b"poke")), base_wire_noun]);
        let poke = T(
            &mut self.context.stack,
            &[event_num, wire, eny.as_noun(), our.as_noun(), now.as_noun(), cause],
        );

        self.do_poke(poke)
    }

    /// Updates the Serf's state after an event.
    ///
    /// # Arguments
    ///
    /// * `new_event_num` - The new event number.
    /// * `new_arvo` - The new Arvo state.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it modifies the Serf's state directly.
    #[tracing::instrument(level = "info", skip_all)]
    pub unsafe fn event_update(&mut self, new_event_num: u64, new_arvo: Noun) {
        self.arvo = new_arvo;
        self.event_num.store(new_event_num, Ordering::SeqCst);

        self.context.cache = Hamt::new(&mut self.context.stack);
        self.context.scry_stack = D(0);
    }

    /// Preserves leftovers after an event update.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it modifies the Serf's state directly.
    #[tracing::instrument(level = "info", skip_all)]
    pub unsafe fn preserve_event_update_leftovers(&mut self) {
        let stack = &mut self.context.stack;
        stack.preserve(&mut self.context.warm);
        stack.preserve(&mut self.context.hot);
        stack.preserve(&mut self.context.cache);
        stack.preserve(&mut self.context.cold);
        stack.preserve(&mut self.arvo);
        stack.flip_top_frame(0);
    }

    /// Returns a mutable reference to the Nock stack.
    ///
    /// # Returns
    ///
    /// A mutable reference to the `NockStack`.
    pub fn stack(&mut self) -> &mut NockStack {
        &mut self.context.stack
    }

    /// Creates a poke swap noun.
    ///
    /// # Arguments
    ///
    /// * `eve` - The event number.
    /// * `mug` - The mug value.
    /// * `ovo` - The original noun.
    /// * `fec` - The effect noun.
    ///
    /// # Returns
    ///
    /// A noun representing the poke swap.
    pub fn poke_bail(&mut self, eve: u64, mug: u64, ovo: Noun, fec: Noun) -> Noun {
        T(
            self.stack(),
            &[D(tas!(b"poke")), D(tas!(b"swap")), D(eve), D(mug), ovo, fec],
        )
    }

    /// Creates a poke bail noun.
    ///
    /// # Arguments
    ///
    /// * `lud` - The lud noun.
    ///
    /// # Returns
    ///
    /// A noun representing the poke bail.
    pub fn poke_bail_noun(&mut self, lud: Noun) -> Noun {
        T(self.stack(), &[D(tas!(b"poke")), D(tas!(b"bail")), lud])
    }
}

fn slot(noun: Noun, axis: u64) -> Result<Noun> {
    Ok(noun.slot(axis)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    async fn setup_kernel(jam: &str) -> (Kernel, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let snap_dir = temp_dir.path().to_path_buf();
        let jam_paths = JamPaths::new(&snap_dir);
        let jam_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("assets")
            .join(jam);
        let jam_bytes =
            fs::read(jam_path).unwrap_or_else(|_| panic!("Failed to read {} file", jam));
        let kernel = Kernel::load(snap_dir, jam_paths, &jam_bytes, false)
            .await
            .expect("Could not load kernel");
        (kernel, temp_dir)
    }

    // Convert this to an integration test and feed it the kernel.jam from Choo in CI/CD
    // https://www.youtube.com/watch?v=4m1EFMoRFvY
    // #[test]
    // #[cfg_attr(miri, ignore)]
    // fn test_kernel_boot() {
    //     let _ = setup_kernel("dumb.jam");
    // }

    // To test your own kernel, place a `kernel.jam` file in the `assets` directory
    // and uncomment the following test:
    //
    // #[test]
    // fn test_custom_kernel() {
    //     let (kernel, _temp_dir) = setup_kernel("kernel.jam");
    //     // Add your custom assertions here to test the kernel's behavior
    // }
}
