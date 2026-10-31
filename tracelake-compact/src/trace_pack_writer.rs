use crate::compactor::CompactError;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::{
    Span, TRACE_PACK_HEADER_LEN, TracePackError, decode_trace_block, decode_trace_pack_header,
    find_trace_block,
};

/// Reads traces back from segment trace packs on the object store.
///
/// A lookup fetches three ranges: the header, the pack directory, and the one
/// block that holds the trace. It never reads the whole pack, so fetch cost
/// follows the trace size rather than the segment size.
#[derive(Clone)]
pub struct TracePackWriter {
    store: Arc<dyn ObjectStore>,
}

impl TracePackWriter {
    /// Creates a reader over one object store.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Reads one trace from a pack object, or `None` when the pack does not
    /// hold the trace.
    pub async fn read_trace(
        &self,
        pack_path: &str,
        trace_id: &str,
    ) -> Result<Option<Vec<Span>>, CompactError> {
        let path = Path::from(pack_path);
        let size = self.store.head(&path).await?.size;

        if size < (TRACE_PACK_HEADER_LEN + 4) as u64 {
            return Err((TracePackError::Truncated { len: size as usize }).into());
        }

        let header_bytes = self
            .store
            .get_range(&path, 0..TRACE_PACK_HEADER_LEN as u64)
            .await?;
        let header = decode_trace_pack_header(&header_bytes)?;

        let directory_end = header
            .directory_offset
            .checked_add(8 * header.trace_count)
            .ok_or(TracePackError::DirectoryOutOfBounds {
                offset: header.directory_offset as u64,
                count: header.trace_count as u64,
                total: size as usize,
            })?;
        if directory_end as u64 > size {
            return Err((TracePackError::DirectoryOutOfBounds {
                offset: header.directory_offset as u64,
                count: header.trace_count as u64,
                total: size as usize,
            })
            .into());
        }

        let directory = self
            .store
            .get_range(&path, header.directory_offset as u64..size)
            .await?;
        let Some(block) = find_trace_block(&directory, &header, trace_id)? else {
            return Ok(None);
        };

        let block_end = block.block_offset.checked_add(block.block_len).ok_or(
            TracePackError::BlockOutOfBounds {
                offset: block.block_offset,
                len: block.block_len,
                total: size as usize,
            },
        )?;
        if block_end > size {
            return Err((TracePackError::BlockOutOfBounds {
                offset: block.block_offset,
                len: block.block_len,
                total: size as usize,
            })
            .into());
        }

        let bytes = self
            .store
            .get_range(&path, block.block_offset..block_end)
            .await?;

        Ok(Some(decode_trace_block(
            &bytes,
            block.span_count,
            block.block_crc32,
        )?))
    }
}
