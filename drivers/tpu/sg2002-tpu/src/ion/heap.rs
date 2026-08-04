//! Ion 堆管理

use alloc::sync::Arc;
use core::alloc::Layout;

use dma_api::{CoherentArray, DeviceDma, DmaError};

use super::{
    error::{IonError, IonResult},
    types::{IonBuffer, IonHeapType},
};

/// Ion 堆管理器
pub struct IonHeapManager {
    dma: DeviceDma,
}

impl IonHeapManager {
    /// 创建新的堆管理器
    pub fn new(dma: DeviceDma) -> Self {
        Self { dma }
    }

    /// 从指定堆分配缓冲区
    pub fn alloc_buffer(
        &self,
        size: usize,
        align: usize,
        heap_type: IonHeapType,
    ) -> IonResult<Arc<IonBuffer>> {
        debug!(
            "Allocating Ion buffer: size={}, align={}, heap_type={:?}",
            size, align, heap_type
        );
        // 校验参数
        if size == 0 {
            return Err(IonError::InvalidArg);
        }

        let dma = match heap_type {
            IonHeapType::System => {
                // 系统堆使用普通的 DMA 内存
                self.alloc_dma_buffer(size, align)?
            }
            IonHeapType::DmaCoherent => {
                // DMA coherent 堆
                self.alloc_dma_buffer(size, align)?
            }
            IonHeapType::Carveout => {
                return Err(IonError::NotSupported);
            }
        };

        let buffer = Arc::new(IonBuffer::new(dma, size));
        debug!("Allocated Ion buffer with handle: {:?}", buffer.handle);

        Ok(buffer)
    }

    /// 分配 DMA 内存
    fn alloc_dma_buffer(&self, size: usize, align: usize) -> IonResult<CoherentArray<u8>> {
        Layout::from_size_align(size, align).map_err(|_| IonError::InvalidArg)?;
        self.dma
            .coherent_array_zero_with_align(size, align)
            .map_err(|err| match err {
                DmaError::LayoutError(_) => IonError::InvalidArg,
                _ => IonError::NoMemory,
            })
    }
}
