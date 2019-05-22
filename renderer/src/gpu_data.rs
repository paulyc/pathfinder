// pathfinder/renderer/src/gpu_data.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Packed data ready to be sent to the GPU.

use crate::options::BoundingQuad;
use crate::tile_map::DenseTileMap;
use pathfinder_geometry::basic::line_segment::{LineSegmentU4, LineSegmentU8};
use pathfinder_geometry::basic::point::Point2DI32;
use pathfinder_geometry::basic::rect::RectF32;
use std::fmt::{Debug, Formatter, Result as DebugResult};
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct BuiltObject {
    pub bounds: RectF32,
    pub fills: Vec<FillBatchPrimitive>,
    pub alpha_tiles: Vec<AlphaTileBatchPrimitive>,
    pub tiles: DenseTileMap<TileObjectPrimitive>,
}

pub enum RenderCommand {
    Start { path_count: usize, bounding_quad: BoundingQuad },
    AddPaintData(PaintMetadata, PaintData),
    AddFills(Vec<FillBatchPrimitive>),
    FlushFills,
    AlphaTile(Vec<AlphaTileBatchPrimitive>),
    SolidTile(Vec<SolidTileBatchPrimitive>),
    Finish { build_time: Duration },
}

#[derive(Clone, Debug)]
pub struct PaintMetadata {
    pub size: Point2DI32,
    pub texels: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct PaintData {
    pub size: Point2DI32,
    pub texels: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct FillObjectPrimitive {
    pub px: LineSegmentU4,
    pub subpx: LineSegmentU8,
    pub tile_x: i16,
    pub tile_y: i16,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct TileObjectPrimitive {
    /// If `u16::MAX`, then this is a solid tile.
    pub alpha_tile_index: u16,
    pub backdrop: i8,
}

// FIXME(pcwalton): Move `subpx` before `px` and remove `repr(packed)`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(packed)]
pub struct FillBatchPrimitive {
    pub px: LineSegmentU4,
    pub subpx: LineSegmentU8,
    pub alpha_tile_index: u16,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct SolidTileBatchPrimitive {
    pub tile_x: i16,
    pub tile_y: i16,
    pub object_index: u16,
}

// FIXME(pcwalton): Maybe move the gradient to be a LUT texture keyed from object ID?
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct AlphaTileBatchPrimitive {
    pub tile_x_lo: u8,
    pub tile_y_lo: u8,
    pub tile_hi: u8,
    pub backdrop: i8,
    pub tile_index: u16,
    pub object_index: u16,
}

impl Debug for RenderCommand {
    fn fmt(&self, formatter: &mut Formatter) -> DebugResult {
        match *self {
            RenderCommand::Start { .. } => write!(formatter, "Start"),
            RenderCommand::AddPaintData(ref paint_metadata, ref paint_data) => {
                write!(formatter,
                       "AddPaintData({}x{}, {}x{})",
                       paint_metadata.size.x(), paint_metadata.size.y(),
                       paint_data.size.x(), paint_data.size.y())
            }
            RenderCommand::AddFills(ref fills) => write!(formatter, "AddFills(x{})", fills.len()),
            RenderCommand::FlushFills => write!(formatter, "FlushFills"),
            RenderCommand::AlphaTile(ref tiles) => {
                write!(formatter, "AlphaTile(x{})", tiles.len())
            }
            RenderCommand::SolidTile(ref tiles) => {
                write!(formatter, "SolidTile(x{})", tiles.len())
            }
            RenderCommand::Finish { .. } => write!(formatter, "Finish"),
        }
    }
}
