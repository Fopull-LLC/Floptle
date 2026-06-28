//! The render graph: passes declare the resources they read and write, the graph
//! orders them and owns the transient textures (`docs/subsystems/renderer.md`).
//!
//! This is the **seam** for that system — `Pass`, `ResourceId`, `RenderGraph` —
//! so the signature look (SDF/raymarch → feedback/echo post → present) can be
//! expressed as declared passes rather than the hand-wired encoder chain the proof
//! uses today. The executor (allocate transients, topo-sort, record into one
//! command encoder) is the Phase-4 fill-in; the trait that passes implement is the
//! part worth pinning now so subsystems can author against it.

use crate::device::Gpu;

/// Handle to a graph-managed texture/buffer (a transient render target, or an
/// imported resource like the swapchain image).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u32);

/// What a pass needs from the graph when it records: the GPU, the command
/// encoder to record into, and a way to resolve declared resources to views.
pub struct PassContext<'a> {
    pub gpu: &'a Gpu,
    pub encoder: &'a mut wgpu::CommandEncoder,
}

/// One unit of rendering work. Passes declare their I/O so the graph can order
/// them and reuse transient memory; `record` does the actual encoding.
pub trait Pass {
    /// Human-readable label (debug groups, profiler rows).
    fn name(&self) -> &str;
    /// Resources this pass samples/reads.
    fn reads(&self) -> &[ResourceId] {
        &[]
    }
    /// Resources this pass writes/produces.
    fn writes(&self) -> &[ResourceId] {
        &[]
    }
    /// Encode this pass's commands. Called by the executor in dependency order.
    fn record(&self, ctx: &mut PassContext<'_>);
}

/// Owns the registered passes and (eventually) the transient resource pool.
#[derive(Default)]
pub struct RenderGraph {
    passes: Vec<Box<dyn Pass>>,
}

impl RenderGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pass. (Ordering is currently insertion order; the dependency
    /// topo-sort from `reads`/`writes` is the Phase-4 fill-in.)
    pub fn add_pass(&mut self, pass: impl Pass + 'static) {
        self.passes.push(Box::new(pass));
    }

    pub fn passes(&self) -> impl Iterator<Item = &dyn Pass> {
        self.passes.iter().map(|p| p.as_ref())
    }

    /// Record every pass into one command encoder and submit.
    ///
    /// Fill-in (Phase 4): allocate transients from `reads`/`writes`, topo-sort by
    /// dependency, then record. For now it records in insertion order.
    pub fn execute(&self, gpu: &Gpu) {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        for pass in &self.passes {
            let mut ctx = PassContext { gpu, encoder: &mut encoder };
            pass.record(&mut ctx);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
