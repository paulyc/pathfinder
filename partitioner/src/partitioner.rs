// pathfinder/partitioner/src/partitioner.rs
//
// Copyright © 2017 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use bit_vec::BitVec;
use euclid::approxeq::ApproxEq;
use euclid::Point2D;
use log::LogLevel;
use pathfinder_path_utils::PathBuffer;
use pathfinder_path_utils::curve::Curve;
use pathfinder_path_utils::line::Line;
use std::collections::BinaryHeap;
use std::cmp::Ordering;
use std::f32;
use std::iter;
use std::u32;

use mesh_library::{MeshLibrary, MeshLibraryIndexRanges};
use {BQuad, BVertexLoopBlinnData, BVertexKind, Endpoint, FillRule, Subpath};

const MAX_B_QUAD_SUBDIVISIONS: u8 = 8;

pub struct Partitioner<'a> {
    endpoints: &'a [Endpoint],
    control_points: &'a [Point2D<f32>],
    subpaths: &'a [Subpath],

    library: MeshLibrary,

    fill_rule: FillRule,

    heap: BinaryHeap<Point>,
    visited_points: BitVec,
    active_edges: Vec<ActiveEdge>,
    path_id: u16,
}

impl<'a> Partitioner<'a> {
    #[inline]
    pub fn new<'b>(library: MeshLibrary) -> Partitioner<'b> {
        Partitioner {
            endpoints: &[],
            control_points: &[],
            subpaths: &[],

            fill_rule: FillRule::Winding,

            library: library,

            heap: BinaryHeap::new(),
            visited_points: BitVec::new(),
            active_edges: vec![],
            path_id: 0,
        }
    }

    #[inline]
    pub fn library(&self) -> &MeshLibrary {
        &self.library
    } 

    #[inline]
    pub fn library_mut(&mut self) -> &mut MeshLibrary {
        &mut self.library
    }

    #[inline]
    pub fn into_library(self) -> MeshLibrary {
        self.library
    }

    pub fn init_with_raw_data(&mut self,
                              new_endpoints: &'a [Endpoint],
                              new_control_points: &'a [Point2D<f32>],
                              new_subpaths: &'a [Subpath]) {
        self.endpoints = new_endpoints;
        self.control_points = new_control_points;
        self.subpaths = new_subpaths;

        // FIXME(pcwalton): Move this initialization to `partition` below. Right now, this bit
        // vector uses too much memory.
        self.visited_points = BitVec::from_elem(self.endpoints.len(), false);
    }

    pub fn init_with_path_buffer(&mut self, path_buffer: &'a PathBuffer) {
        self.init_with_raw_data(&path_buffer.endpoints,
                                &path_buffer.control_points,
                                &path_buffer.subpaths)
    }

    #[inline]
    pub fn set_fill_rule(&mut self, new_fill_rule: FillRule) {
        self.fill_rule = new_fill_rule
    }

    pub fn partition(&mut self, path_id: u16, first_subpath_index: u32, last_subpath_index: u32)
                     -> MeshLibraryIndexRanges {
        self.heap.clear();
        self.active_edges.clear();

        let start_lengths = self.library.snapshot_lengths();

        self.path_id = path_id;

        self.init_heap(first_subpath_index, last_subpath_index);

        while self.process_next_point() {}

        debug_assert!(self.library.b_vertex_loop_blinn_data.len() ==
                      self.library.b_vertex_path_ids.len());
        debug_assert!(self.library.b_vertex_loop_blinn_data.len() ==
                      self.library.b_vertex_positions.len());

        let end_lengths = self.library.snapshot_lengths();
        MeshLibraryIndexRanges::new(&start_lengths, &end_lengths)
    }

    fn process_next_point(&mut self) -> bool {
        let point = match self.heap.peek() {
            Some(point) => *point,
            None => return false,
        };

        if self.already_visited_point(&point) {
            self.heap.pop();
            return true
        }

        if log_enabled!(LogLevel::Debug) {
            let position = self.endpoints[point.endpoint_index as usize].position;
            debug!("processing point {}: {:?}", point.endpoint_index, position);
            debug!("... active edges at {}:", position.x);
            for (active_edge_index, active_edge) in self.active_edges.iter().enumerate() {
                let y = self.solve_active_edge_y_for_x(position.x, active_edge);
                debug!("... ... edge {}: {:?} @ ({}, {})",
                       active_edge_index,
                       active_edge,
                       position.x,
                       y);
            }
        }

        self.mark_point_as_visited(&point);

        self.sort_active_edge_list_and_emit_self_intersections(point.endpoint_index);

        let matching_active_edges = self.find_right_point_in_active_edge_list(point.endpoint_index);
        match matching_active_edges.count {
            0 => self.process_min_endpoint(point.endpoint_index),
            1 => {
                self.process_regular_endpoint(point.endpoint_index,
                                              matching_active_edges.indices[0])
            }
            2 => self.process_max_endpoint(point.endpoint_index, matching_active_edges.indices),
            _ => debug_assert!(false),
        }

        true
    }

    fn process_min_endpoint(&mut self, endpoint_index: u32) {
        debug!("... MIN point");

        let next_active_edge_index = self.find_point_between_active_edges(endpoint_index);

        let endpoint = &self.endpoints[endpoint_index as usize];
        self.emit_b_quads_around_active_edge(next_active_edge_index, endpoint.position.x);

        self.add_new_edges_for_min_point(endpoint_index, next_active_edge_index);

        let prev_endpoint_index = self.prev_endpoint_of(endpoint_index);
        let next_endpoint_index = self.next_endpoint_of(endpoint_index);
        let new_point = self.create_point_from_endpoint(next_endpoint_index);
        *self.heap.peek_mut().unwrap() = new_point;
        if next_endpoint_index != prev_endpoint_index {
            let new_point = self.create_point_from_endpoint(prev_endpoint_index);
            self.heap.push(new_point)
        }
    }

    fn process_regular_endpoint(&mut self, endpoint_index: u32, active_edge_index: u32) {
        debug!("... REGULAR point: active edge {}", active_edge_index);

        let endpoint = &self.endpoints[endpoint_index as usize];
        let bottom = self.emit_b_quads_around_active_edge(active_edge_index, endpoint.position.x) ==
            BQuadEmissionResult::BQuadEmittedAbove;

        let prev_endpoint_index = self.prev_endpoint_of(endpoint_index);
        let next_endpoint_index = self.next_endpoint_of(endpoint_index);

        {
            let active_edge = &mut self.active_edges[active_edge_index as usize];
            let endpoint_position = self.endpoints[active_edge.right_endpoint_index as usize]
                                        .position;

            // If we already made a B-vertex point for this endpoint, reuse it instead of making a
            // new one.
            let old_left_position =
                self.library.b_vertex_positions[active_edge.left_vertex_index as usize];
            let should_update = (endpoint_position - old_left_position).square_length() >
                f32::approx_epsilon();
            if should_update {
                active_edge.left_vertex_index = self.library.b_vertex_loop_blinn_data.len() as u32;
                active_edge.control_point_vertex_index = active_edge.left_vertex_index + 1;

                self.library.b_vertex_positions.push(endpoint_position);
                self.library.b_vertex_path_ids.push(self.path_id);
                self.library.b_vertex_loop_blinn_data.push(BVertexLoopBlinnData::new(
                    active_edge.endpoint_kind()));

                active_edge.toggle_parity();
            }

            if active_edge.left_to_right {
                active_edge.right_endpoint_index = next_endpoint_index;
            } else {
                active_edge.right_endpoint_index = prev_endpoint_index;
            }
        }

        let right_endpoint_index = self.active_edges[active_edge_index as usize]
                                       .right_endpoint_index;
        let new_point = self.create_point_from_endpoint(right_endpoint_index);
        *self.heap.peek_mut().unwrap() = new_point;

        let control_point_index = if self.active_edges[active_edge_index as usize].left_to_right {
            self.control_point_index_before_endpoint(next_endpoint_index)
        } else {
            self.control_point_index_after_endpoint(prev_endpoint_index)
        };

        match control_point_index {
            u32::MAX => {
                self.active_edges[active_edge_index as usize].control_point_vertex_index = u32::MAX
            }
            control_point_index => {
                self.active_edges[active_edge_index as usize].control_point_vertex_index =
                    self.library.b_vertex_loop_blinn_data.len() as u32;

                let left_vertex_index = self.active_edges[active_edge_index as usize]
                                            .left_vertex_index;
                let control_point_position = &self.control_points[control_point_index as usize];
                let control_point_b_vertex_loop_blinn_data = BVertexLoopBlinnData::control_point(
                    &self.library.b_vertex_positions[left_vertex_index as usize],
                    &control_point_position,
                    &new_point.position,
                    bottom);
                self.library.b_vertex_positions.push(*control_point_position);
                self.library.b_vertex_path_ids.push(self.path_id);
                self.library.b_vertex_loop_blinn_data.push(control_point_b_vertex_loop_blinn_data);
            }
        }
    }

    fn process_max_endpoint(&mut self, endpoint_index: u32, active_edge_indices: [u32; 2]) {
        debug!("... MAX point: active edges {:?}", active_edge_indices);

        debug_assert!(active_edge_indices[0] < active_edge_indices[1],
                      "Matching active edge indices in wrong order when processing MAX point");

        let endpoint = &self.endpoints[endpoint_index as usize];

        // TODO(pcwalton): Collapse the two duplicate endpoints that this will create together if
        // possible (i.e. if they have the same parity).
        self.emit_b_quads_around_active_edge(active_edge_indices[0], endpoint.position.x);
        self.emit_b_quads_around_active_edge(active_edge_indices[1], endpoint.position.x);

        self.heap.pop();

        // FIXME(pcwalton): This is twice as slow as it needs to be.
        self.active_edges.remove(active_edge_indices[1] as usize);
        self.active_edges.remove(active_edge_indices[0] as usize);
    }

    fn sort_active_edge_list_and_emit_self_intersections(&mut self, endpoint_index: u32) {
        let x = self.endpoints[endpoint_index as usize].position.x;
        loop {
            let mut swapped = false;
            for lower_active_edge_index in 1..(self.active_edges.len() as u32) {
                let upper_active_edge_index = lower_active_edge_index - 1;

                if self.active_edges_are_ordered(upper_active_edge_index,
                                                 lower_active_edge_index,
                                                 x) {
                    continue
                }

                if let Some(crossing_point) =
                        self.crossing_point_for_active_edge(upper_active_edge_index, x) {
                    debug!("found SELF-INTERSECTION point for active edges {} & {}",
                           upper_active_edge_index,
                           lower_active_edge_index);
                    self.emit_b_quads_around_active_edge(upper_active_edge_index, crossing_point.x);
                    self.emit_b_quads_around_active_edge(lower_active_edge_index, crossing_point.x);
                } else {
                    debug!("warning: swapped active edges {} & {} without finding intersection",
                           upper_active_edge_index,
                           lower_active_edge_index);
                }

                self.active_edges.swap(upper_active_edge_index as usize,
                                       lower_active_edge_index as usize);
                swapped = true;
            }

            if !swapped {
                break
            }
        }
    }

    fn add_new_edges_for_min_point(&mut self, endpoint_index: u32, next_active_edge_index: u32) {
        // FIXME(pcwalton): This is twice as slow as it needs to be.
        self.active_edges.insert(next_active_edge_index as usize, ActiveEdge::default());
        self.active_edges.insert(next_active_edge_index as usize, ActiveEdge::default());

        let prev_endpoint_index = self.prev_endpoint_of(endpoint_index);
        let next_endpoint_index = self.next_endpoint_of(endpoint_index);

        let new_active_edges = &mut self.active_edges[next_active_edge_index as usize..
                                                      next_active_edge_index as usize + 2];

        let left_vertex_index = self.library.b_vertex_loop_blinn_data.len() as u32;
        new_active_edges[0].left_vertex_index = left_vertex_index;
        new_active_edges[1].left_vertex_index = left_vertex_index;

        let position = self.endpoints[endpoint_index as usize].position;
        self.library.b_vertex_positions.push(position);
        self.library.b_vertex_path_ids.push(self.path_id);
        self.library.b_vertex_loop_blinn_data
            .push(BVertexLoopBlinnData::new(BVertexKind::Endpoint0));

        new_active_edges[0].toggle_parity();
        new_active_edges[1].toggle_parity();

        let endpoint = &self.endpoints[endpoint_index as usize];
        let prev_endpoint = &self.endpoints[prev_endpoint_index as usize];
        let next_endpoint = &self.endpoints[next_endpoint_index as usize];

        let prev_vector = prev_endpoint.position - endpoint.position;
        let next_vector = next_endpoint.position - endpoint.position;

        let (upper_control_point_index, lower_control_point_index);
        if prev_vector.cross(next_vector) >= 0.0 {
            new_active_edges[0].right_endpoint_index = prev_endpoint_index;
            new_active_edges[1].right_endpoint_index = next_endpoint_index;
            new_active_edges[0].left_to_right = false;
            new_active_edges[1].left_to_right = true;

            upper_control_point_index = self.endpoints[endpoint_index as usize].control_point_index;
            lower_control_point_index = self.endpoints[next_endpoint_index as usize]
                                            .control_point_index;
        } else {
            new_active_edges[0].right_endpoint_index = next_endpoint_index;
            new_active_edges[1].right_endpoint_index = prev_endpoint_index;
            new_active_edges[0].left_to_right = true;
            new_active_edges[1].left_to_right = false;

            upper_control_point_index = self.endpoints[next_endpoint_index as usize]
                                            .control_point_index;
            lower_control_point_index = self.endpoints[endpoint_index as usize].control_point_index;
        }

        match upper_control_point_index {
            u32::MAX => new_active_edges[0].control_point_vertex_index = u32::MAX,
            upper_control_point_index => {
                new_active_edges[0].control_point_vertex_index =
                    self.library.b_vertex_loop_blinn_data.len() as u32;

                let control_point_position =
                    self.control_points[upper_control_point_index as usize];
                let right_vertex_position =
                    self.endpoints[new_active_edges[0].right_endpoint_index as usize].position;
                let control_point_b_vertex_loop_blinn_data =
                    BVertexLoopBlinnData::control_point(&position,
                                                        &control_point_position,
                                                        &right_vertex_position,
                                                        false);
                self.library.b_vertex_positions.push(control_point_position);
                self.library.b_vertex_path_ids.push(self.path_id);
                self.library.b_vertex_loop_blinn_data.push(control_point_b_vertex_loop_blinn_data);
            }
        }

        match lower_control_point_index {
            u32::MAX => new_active_edges[1].control_point_vertex_index = u32::MAX,
            lower_control_point_index => {
                new_active_edges[1].control_point_vertex_index =
                    self.library.b_vertex_loop_blinn_data.len() as u32;

                let control_point_position =
                    self.control_points[lower_control_point_index as usize];
                let right_vertex_position =
                    self.endpoints[new_active_edges[1].right_endpoint_index as usize].position;
                let control_point_b_vertex_loop_blinn_data =
                    BVertexLoopBlinnData::control_point(&position,
                                                        &control_point_position,
                                                        &right_vertex_position,
                                                        true);
                self.library.b_vertex_positions.push(control_point_position);
                self.library.b_vertex_path_ids.push(self.path_id);
                self.library.b_vertex_loop_blinn_data.push(control_point_b_vertex_loop_blinn_data);
            }
        }
    }

    fn active_edges_are_ordered(&mut self,
                                prev_active_edge_index: u32,
                                next_active_edge_index: u32,
                                x: f32)
                                -> bool {
        let prev_active_edge = &self.active_edges[prev_active_edge_index as usize];
        let next_active_edge = &self.active_edges[next_active_edge_index as usize];
        if prev_active_edge.right_endpoint_index == next_active_edge.right_endpoint_index {
            // Always ordered.
            // FIXME(pcwalton): Is this true?
            return true
        }

        // TODO(pcwalton): See if we can speed this up. It's trickier than it seems, due to path
        // self intersection!
        let prev_active_edge_t = self.solve_active_edge_t_for_x(x, prev_active_edge);
        let next_active_edge_t = self.solve_active_edge_t_for_x(x, next_active_edge);
        let prev_active_edge_y = self.sample_active_edge(prev_active_edge_t, prev_active_edge).y;
        let next_active_edge_y = self.sample_active_edge(next_active_edge_t, next_active_edge).y;
        prev_active_edge_y <= next_active_edge_y
    }

    fn init_heap(&mut self, first_subpath_index: u32, last_subpath_index: u32) {
        for subpath in &self.subpaths[(first_subpath_index as usize)..
                                      (last_subpath_index as usize)] {
            for endpoint_index in subpath.first_endpoint_index..subpath.last_endpoint_index {
                match self.classify_endpoint(endpoint_index) {
                    EndpointClass::Min => {
                        let new_point = self.create_point_from_endpoint(endpoint_index);
                        self.heap.push(new_point)
                    }
                    EndpointClass::Regular | EndpointClass::Max => {}
                }
            }
        }
    }

    fn bounding_active_edges_for_fill(&self, active_edge_index: u32) -> (u32, u32) {
        match self.fill_rule {
            FillRule::EvenOdd if active_edge_index % 2 == 1 => {
                (active_edge_index - 1, active_edge_index)
            }
            FillRule::EvenOdd if (active_edge_index as usize) + 1 == self.active_edges.len() => {
                (active_edge_index, active_edge_index)
            }
            FillRule::EvenOdd => (active_edge_index, active_edge_index + 1),

            FillRule::Winding => {
                let (mut winding_number, mut upper_active_edge_index) = (0, 0);
                for (active_edge_index, active_edge) in
                        self.active_edges[0..active_edge_index as usize].iter().enumerate() {
                    if winding_number == 0 {
                        upper_active_edge_index = active_edge_index as u32
                    }
                    winding_number += active_edge.winding_number()
                }
                if winding_number == 0 {
                    upper_active_edge_index = active_edge_index as u32
                }

                let mut lower_active_edge_index = active_edge_index;
                for (active_edge_index, active_edge) in
                        self.active_edges.iter().enumerate().skip(active_edge_index as usize) {
                    winding_number += active_edge.winding_number();
                    if winding_number == 0 {
                        lower_active_edge_index = active_edge_index as u32;
                        break
                    }
                }

                (upper_active_edge_index, lower_active_edge_index)
            }
        }
    }

    fn emit_b_quads_around_active_edge(&mut self, active_edge_index: u32, right_x: f32)
                                       -> BQuadEmissionResult {
        if (active_edge_index as usize) >= self.active_edges.len() {
            return BQuadEmissionResult::NoBQuadEmitted
        }

        // TODO(pcwalton): Assert that the green X position is the same on both edges.
        let (upper_active_edge_index, lower_active_edge_index) =
            self.bounding_active_edges_for_fill(active_edge_index);
        debug!("... bounding active edges for fill = [{},{}] around {}",
               upper_active_edge_index,
               lower_active_edge_index,
               active_edge_index);

        let emission_result = BQuadEmissionResult::new(active_edge_index,
                                                       upper_active_edge_index,
                                                       lower_active_edge_index);
        if emission_result == BQuadEmissionResult::NoBQuadEmitted {
            return emission_result
        }

        if !self.should_subdivide_active_edge_at(upper_active_edge_index, right_x) ||
                !self.should_subdivide_active_edge_at(lower_active_edge_index, right_x) {
            return emission_result
        }

        let upper_curve = self.subdivide_active_edge_at(upper_active_edge_index,
                                                        right_x,
                                                        SubdivisionType::Upper);
        for active_edge_index in (upper_active_edge_index + 1)..lower_active_edge_index {
            if self.should_subdivide_active_edge_at(active_edge_index, right_x) {
                self.subdivide_active_edge_at(active_edge_index, right_x, SubdivisionType::Inside);
                self.active_edges[active_edge_index as usize].toggle_parity();
            }
        }
        let lower_curve = self.subdivide_active_edge_at(lower_active_edge_index,
                                                        right_x,
                                                        SubdivisionType::Lower);

        self.emit_b_quads(upper_active_edge_index,
                          lower_active_edge_index,
                          &upper_curve,
                          &lower_curve,
                          0);

        emission_result
    }

    /// Toggles parity at the end.
    fn emit_b_quads(&mut self,
                    upper_active_edge_index: u32,
                    lower_active_edge_index: u32,
                    upper_subdivision: &SubdividedActiveEdge,
                    lower_subdivision: &SubdividedActiveEdge,
                    iteration: u8) {
        let upper_shape = upper_subdivision.shape(&self.library.b_vertex_loop_blinn_data);
        let lower_shape = lower_subdivision.shape(&self.library.b_vertex_loop_blinn_data);

        // Make sure the convex hulls of the two curves do not intersect. If they do, subdivide and
        // recurse.
        if iteration < MAX_B_QUAD_SUBDIVISIONS {
            // TODO(pcwalton): Handle concave-line convex hull intersections.
            if let (Some(upper_curve), Some(lower_curve)) =
                    (upper_subdivision.to_curve(&self.library.b_vertex_positions),
                     lower_subdivision.to_curve(&self.library.b_vertex_positions)) {
                // TODO(pcwalton): Handle concave-concave convex hull intersections.
                if upper_shape == Shape::Concave &&
                        lower_curve.baseline().side(&upper_curve.control_point) >
                        f32::approx_epsilon() {
                    let (upper_left_subsubdivision, upper_right_subsubdivision) =
                        self.subdivide_active_edge_again_at_t(&upper_subdivision,
                                                              0.5,
                                                              false);
                    let midpoint_x =
                        self.library
                            .b_vertex_positions[upper_left_subsubdivision.middle_point as usize].x;
                    let (lower_left_subsubdivision, lower_right_subsubdivision) =
                        self.subdivide_active_edge_again_at_x(&lower_subdivision,
                                                              midpoint_x,
                                                              true);

                    self.emit_b_quads(upper_active_edge_index,
                                      lower_active_edge_index,
                                      &upper_left_subsubdivision,
                                      &lower_left_subsubdivision,
                                      iteration + 1);
                    self.emit_b_quads(upper_active_edge_index,
                                      lower_active_edge_index,
                                      &upper_right_subsubdivision,
                                      &lower_right_subsubdivision,
                                      iteration + 1);
                    return;
                }

                if lower_shape == Shape::Concave &&
                        upper_curve.baseline().side(&lower_curve.control_point) <
                        -f32::approx_epsilon() {
                    let (lower_left_subsubdivision, lower_right_subsubdivision) =
                        self.subdivide_active_edge_again_at_t(&lower_subdivision,
                                                              0.5,
                                                              true);
                    let midpoint_x =
                        self.library
                            .b_vertex_positions[lower_left_subsubdivision.middle_point as usize].x;
                    let (upper_left_subsubdivision, upper_right_subsubdivision) =
                        self.subdivide_active_edge_again_at_x(&upper_subdivision,
                                                              midpoint_x,
                                                              false);

                    self.emit_b_quads(upper_active_edge_index,
                                      lower_active_edge_index,
                                      &upper_left_subsubdivision,
                                      &lower_left_subsubdivision,
                                      iteration + 1);
                    self.emit_b_quads(upper_active_edge_index,
                                      lower_active_edge_index,
                                      &upper_right_subsubdivision,
                                      &lower_right_subsubdivision,
                                      iteration + 1);
                    return;
                }
            }
        }

        debug!("... emitting B-quad: UL {} BL {} UR {} BR {}",
               upper_subdivision.left_curve_left,
               lower_subdivision.left_curve_left,
               upper_subdivision.middle_point,
               lower_subdivision.middle_point);

        {
            let upper_active_edge = &mut self.active_edges[upper_active_edge_index as usize];
            self.library.b_vertex_loop_blinn_data[upper_subdivision.middle_point as usize] =
                BVertexLoopBlinnData::new(upper_active_edge.endpoint_kind());
            upper_active_edge.toggle_parity();
        }
        {
            let lower_active_edge = &mut self.active_edges[lower_active_edge_index as usize];
            self.library.b_vertex_loop_blinn_data[lower_subdivision.middle_point as usize] =
                BVertexLoopBlinnData::new(lower_active_edge.endpoint_kind());
            lower_active_edge.toggle_parity();
        }

        match (upper_shape, lower_shape) {
            (Shape::Flat, Shape::Flat) |
            (Shape::Flat, Shape::Convex) |
            (Shape::Convex, Shape::Flat) |
            (Shape::Convex, Shape::Convex) => {
                self.library.cover_indices.interior_indices.extend([
                    upper_subdivision.left_curve_left,
                    upper_subdivision.middle_point,
                    lower_subdivision.left_curve_left,
                    lower_subdivision.middle_point,
                    lower_subdivision.left_curve_left,
                    upper_subdivision.middle_point,
                ].into_iter());
                if upper_shape != Shape::Flat {
                    self.library.cover_indices.curve_indices.extend([
                        upper_subdivision.left_curve_control_point,
                        upper_subdivision.middle_point,
                        upper_subdivision.left_curve_left,
                    ].into_iter())
                }
                if lower_shape != Shape::Flat {
                    self.library.cover_indices.curve_indices.extend([
                        lower_subdivision.left_curve_control_point,
                        lower_subdivision.left_curve_left,
                        lower_subdivision.middle_point,
                    ].into_iter())
                }
            }

            (Shape::Concave, Shape::Flat) |
            (Shape::Concave, Shape::Convex) => {
                self.library.cover_indices.interior_indices.extend([
                    upper_subdivision.left_curve_left,
                    upper_subdivision.left_curve_control_point,
                    lower_subdivision.left_curve_left,
                    upper_subdivision.middle_point,
                    lower_subdivision.middle_point,
                    upper_subdivision.left_curve_control_point,
                    lower_subdivision.middle_point,
                    lower_subdivision.left_curve_left,
                    upper_subdivision.left_curve_control_point,
                ].into_iter());
                self.library.cover_indices.curve_indices.extend([
                    upper_subdivision.left_curve_control_point,
                    upper_subdivision.left_curve_left,
                    upper_subdivision.middle_point,
                ].into_iter());
                if lower_shape != Shape::Flat {
                    self.library.cover_indices.curve_indices.extend([
                        lower_subdivision.left_curve_control_point,
                        lower_subdivision.left_curve_left,
                        lower_subdivision.middle_point,
                    ].into_iter())
                }
            }

            (Shape::Flat, Shape::Concave) |
            (Shape::Convex, Shape::Concave) => {
                self.library.cover_indices.interior_indices.extend([
                    upper_subdivision.left_curve_left,
                    upper_subdivision.middle_point,
                    lower_subdivision.left_curve_control_point,
                    upper_subdivision.middle_point,
                    lower_subdivision.middle_point,
                    lower_subdivision.left_curve_control_point,
                    upper_subdivision.left_curve_left,
                    lower_subdivision.left_curve_control_point,
                    lower_subdivision.left_curve_left,
                ].into_iter());
                self.library.cover_indices.curve_indices.extend([
                    lower_subdivision.left_curve_control_point,
                    lower_subdivision.middle_point,
                    lower_subdivision.left_curve_left,
                ].into_iter());
                if upper_shape != Shape::Flat {
                    self.library.cover_indices.curve_indices.extend([
                        upper_subdivision.left_curve_control_point,
                        upper_subdivision.middle_point,
                        upper_subdivision.left_curve_left,
                    ].into_iter())
                }
            }

            (Shape::Concave, Shape::Concave) => {
                self.library.cover_indices.interior_indices.extend([
                    upper_subdivision.left_curve_left,
                    upper_subdivision.left_curve_control_point,
                    lower_subdivision.left_curve_left,
                    lower_subdivision.left_curve_left,
                    upper_subdivision.left_curve_control_point,
                    lower_subdivision.left_curve_control_point,
                    upper_subdivision.middle_point,
                    lower_subdivision.left_curve_control_point,
                    upper_subdivision.left_curve_control_point,
                    upper_subdivision.middle_point,
                    lower_subdivision.middle_point,
                    lower_subdivision.left_curve_control_point,
                ].into_iter());
                self.library.cover_indices.curve_indices.extend([
                    upper_subdivision.left_curve_control_point,
                    upper_subdivision.left_curve_left,
                    upper_subdivision.middle_point,
                    lower_subdivision.left_curve_control_point,
                    lower_subdivision.middle_point,
                    lower_subdivision.left_curve_left,
                ].into_iter());
            }
        }

        self.library.add_b_quad(&BQuad::new(upper_subdivision.left_curve_left,
                                            upper_subdivision.left_curve_control_point,
                                            upper_subdivision.middle_point,
                                            lower_subdivision.left_curve_left,
                                            lower_subdivision.left_curve_control_point,
                                            lower_subdivision.middle_point))
    }

    fn subdivide_active_edge_again_at_t(&mut self,
                                        subdivision: &SubdividedActiveEdge,
                                        t: f32,
                                        bottom: bool)
                                        -> (SubdividedActiveEdge, SubdividedActiveEdge) {
        let curve = subdivision.to_curve(&self.library.b_vertex_positions)
                               .expect("subdivide_active_edge_again_at_t(): not a curve!");
        let (left_curve, right_curve) = curve.subdivide(t);

        let left_control_point_index = self.library.b_vertex_positions.len() as u32;
        let midpoint_index = left_control_point_index + 1;
        let right_control_point_index = midpoint_index + 1;
        self.library.b_vertex_positions.extend([
            left_curve.control_point,
            left_curve.endpoints[1],
            right_curve.control_point,
        ].into_iter());

        self.library.b_vertex_path_ids.extend(iter::repeat(self.path_id).take(3));

        // Initially, assume that the parity is false. We will modify the Loop-Blinn data later if
        // that is incorrect.
        self.library.b_vertex_loop_blinn_data.extend([
            BVertexLoopBlinnData::control_point(&left_curve.endpoints[0],
                                                &left_curve.control_point,
                                                &left_curve.endpoints[1],
                                                bottom),
            BVertexLoopBlinnData::new(BVertexKind::Endpoint0),
            BVertexLoopBlinnData::control_point(&right_curve.endpoints[0],
                                                &right_curve.control_point,
                                                &right_curve.endpoints[1],
                                                bottom),
        ].into_iter());

        (SubdividedActiveEdge {
            left_curve_left: subdivision.left_curve_left,
            left_curve_control_point: left_control_point_index,
            middle_point: midpoint_index,
        }, SubdividedActiveEdge {
            left_curve_left: midpoint_index,
            left_curve_control_point: right_control_point_index,
            middle_point: subdivision.middle_point,
        })
    }

    fn subdivide_active_edge_again_at_x(&mut self,
                                        subdivision: &SubdividedActiveEdge,
                                        x: f32,
                                        bottom: bool)
                                        -> (SubdividedActiveEdge, SubdividedActiveEdge) {
        let curve = subdivision.to_curve(&self.library.b_vertex_positions)
                               .expect("subdivide_active_edge_again_at_x(): not a curve!");
        let t = curve.solve_t_for_x(x);
        self.subdivide_active_edge_again_at_t(subdivision, t, bottom)
    }

    fn already_visited_point(&self, point: &Point) -> bool {
        // FIXME(pcwalton): This makes the visited vector too big.
        let index = point.endpoint_index as usize;
        match self.visited_points.get(index) {
            None => false,
            Some(visited) => visited,
        }
    }

    fn mark_point_as_visited(&mut self, point: &Point) {
        // FIXME(pcwalton): This makes the visited vector too big.
        self.visited_points.set(point.endpoint_index as usize, true)
    }

    fn find_right_point_in_active_edge_list(&self, endpoint_index: u32) -> MatchingActiveEdges {
        let mut matching_active_edges = MatchingActiveEdges {
            indices: [0, 0],
            count: 0,
        };

        for (active_edge_index, active_edge) in self.active_edges.iter().enumerate() {
            if active_edge.right_endpoint_index == endpoint_index {
                matching_active_edges.indices[matching_active_edges.count as usize] =
                    active_edge_index as u32;
                matching_active_edges.count += 1;
                if matching_active_edges.count == 2 {
                    break
                }
            }
        }

        matching_active_edges
    }

    fn classify_endpoint(&self, endpoint_index: u32) -> EndpointClass {
        // Create temporary points just for the comparison.
        let point = self.create_point_from_endpoint(endpoint_index);
        let prev_point = self.create_point_from_endpoint(self.prev_endpoint_of(endpoint_index));
        let next_point = self.create_point_from_endpoint(self.next_endpoint_of(endpoint_index));

        // Remember to reverse, because the comparison is reversed (as the heap is a max-heap).
        match (prev_point.cmp(&point).reverse(), next_point.cmp(&point).reverse()) {
            (Ordering::Less, Ordering::Less) => EndpointClass::Max,
            (Ordering::Less, _) | (_, Ordering::Less) => EndpointClass::Regular,
            (_, _) => EndpointClass::Min,
        }
    }

    fn find_point_between_active_edges(&self, endpoint_index: u32) -> u32 {
        let endpoint = &self.endpoints[endpoint_index as usize];
        match self.active_edges.iter().position(|active_edge| {
            self.solve_active_edge_y_for_x(endpoint.position.x, active_edge) > endpoint.position.y
        }) {
            Some(active_edge_index) => active_edge_index as u32,
            None => self.active_edges.len() as u32,
        }
    }

    fn solve_active_edge_t_for_x(&self, x: f32, active_edge: &ActiveEdge) -> f32 {
        let left_vertex_position =
            &self.library.b_vertex_positions[active_edge.left_vertex_index as usize];
        let right_endpoint_position = &self.endpoints[active_edge.right_endpoint_index as usize]
                                           .position;
        match active_edge.control_point_vertex_index {
            u32::MAX => Line::new(left_vertex_position, right_endpoint_position).solve_t_for_x(x),
            control_point_vertex_index => {
                let control_point = &self.library
                                         .b_vertex_positions[control_point_vertex_index as usize];
                Curve::new(left_vertex_position,
                           control_point,
                           right_endpoint_position).solve_t_for_x(x)
            }
        }
    }

    fn solve_active_edge_y_for_x(&self, x: f32, active_edge: &ActiveEdge) -> f32 {
        self.sample_active_edge(self.solve_active_edge_t_for_x(x, active_edge), active_edge).y
    }

    fn sample_active_edge(&self, t: f32, active_edge: &ActiveEdge) -> Point2D<f32> {
        let left_vertex_position =
            &self.library.b_vertex_positions[active_edge.left_vertex_index as usize];
        let right_endpoint_position =
            &self.endpoints[active_edge.right_endpoint_index as usize].position;
        match active_edge.control_point_vertex_index {
            u32::MAX => {
                left_vertex_position.to_vector()
                                    .lerp(right_endpoint_position.to_vector(), t)
                                    .to_point()
            }
            control_point_vertex_index => {
                let control_point = &self.library
                                         .b_vertex_positions[control_point_vertex_index as usize];
                Curve::new(left_vertex_position, control_point, right_endpoint_position).sample(t)
            }
        }
    }

    fn crossing_point_for_active_edge(&self, upper_active_edge_index: u32, max_x: f32)
                                      -> Option<Point2D<f32>> {
        let lower_active_edge_index = upper_active_edge_index + 1;

        let upper_active_edge = &self.active_edges[upper_active_edge_index as usize];
        let lower_active_edge = &self.active_edges[lower_active_edge_index as usize];
        if upper_active_edge.left_vertex_index == lower_active_edge.left_vertex_index ||
                upper_active_edge.right_endpoint_index == lower_active_edge.right_endpoint_index {
            return None
        }

        let upper_left_vertex_position =
            &self.library.b_vertex_positions[upper_active_edge.left_vertex_index as usize];
        let upper_right_endpoint_position =
            &self.endpoints[upper_active_edge.right_endpoint_index as usize].position;
        let lower_left_vertex_position =
            &self.library.b_vertex_positions[lower_active_edge.left_vertex_index as usize];
        let lower_right_endpoint_position =
            &self.endpoints[lower_active_edge.right_endpoint_index as usize].position;

        match (upper_active_edge.control_point_vertex_index,
               lower_active_edge.control_point_vertex_index) {
            (u32::MAX, u32::MAX) => {
                let (upper_line, _) =
                    Line::new(upper_left_vertex_position,
                              upper_right_endpoint_position).subdivide_at_x(max_x);
                let (lower_line, _) =
                    Line::new(lower_left_vertex_position,
                              lower_right_endpoint_position).subdivide_at_x(max_x);
                upper_line.intersect_with_line(&lower_line)
            }

            (upper_control_point_vertex_index, u32::MAX) => {
                let upper_control_point =
                    &self.library.b_vertex_positions[upper_control_point_vertex_index as usize];
                let (upper_curve, _) =
                    Curve::new(&upper_left_vertex_position,
                               &upper_control_point,
                               &upper_right_endpoint_position).subdivide_at_x(max_x);
                let (lower_line, _) =
                    Line::new(lower_left_vertex_position,
                              lower_right_endpoint_position).subdivide_at_x(max_x);
                upper_curve.intersect(&lower_line)
            }

            (u32::MAX, lower_control_point_vertex_index) => {
                let lower_control_point =
                    &self.library.b_vertex_positions[lower_control_point_vertex_index as usize];
                let (lower_curve, _) =
                    Curve::new(&lower_left_vertex_position,
                               &lower_control_point,
                               &lower_right_endpoint_position).subdivide_at_x(max_x);
                let (upper_line, _) =
                    Line::new(upper_left_vertex_position,
                              upper_right_endpoint_position).subdivide_at_x(max_x);
                lower_curve.intersect(&upper_line)
            }

            (upper_control_point_vertex_index, lower_control_point_vertex_index) => {
                let upper_control_point =
                    &self.library.b_vertex_positions[upper_control_point_vertex_index as usize];
                let lower_control_point =
                    &self.library.b_vertex_positions[lower_control_point_vertex_index as usize];
                let (upper_curve, _) =
                    Curve::new(&upper_left_vertex_position,
                               &upper_control_point,
                               &upper_right_endpoint_position).subdivide_at_x(max_x);
                let (lower_curve, _) =
                    Curve::new(&lower_left_vertex_position,
                               &lower_control_point,
                               &lower_right_endpoint_position).subdivide_at_x(max_x);
                upper_curve.intersect(&lower_curve)
            }
        }
    }

    fn should_subdivide_active_edge_at(&self, active_edge_index: u32, x: f32) -> bool {
        let left_curve_left = self.active_edges[active_edge_index as usize].left_vertex_index;
        let left_point_position = self.library.b_vertex_positions[left_curve_left as usize];
        x - left_point_position.x >= f32::approx_epsilon()
    }

    /// Does *not* toggle parity. You must do this after calling this function.
    fn subdivide_active_edge_at(&mut self,
                                active_edge_index: u32,
                                x: f32,
                                subdivision_type: SubdivisionType)
                                -> SubdividedActiveEdge {
        let left_curve_left = self.active_edges[active_edge_index as usize].left_vertex_index;
        let left_point_position = self.library.b_vertex_positions[left_curve_left as usize];

        let t = self.solve_active_edge_t_for_x(x, &self.active_edges[active_edge_index as usize]);

        let bottom = subdivision_type == SubdivisionType::Lower;
        let active_edge = &mut self.active_edges[active_edge_index as usize];

        let left_curve_control_point_vertex_index;
        match active_edge.control_point_vertex_index {
            u32::MAX => {
                let path_id = self.library.b_vertex_path_ids[left_curve_left as usize];
                let right_point = self.endpoints[active_edge.right_endpoint_index as usize]
                                      .position;
                let middle_point = left_point_position.to_vector().lerp(right_point.to_vector(), t);

                active_edge.left_vertex_index = self.library.b_vertex_loop_blinn_data.len() as u32;
                self.library.b_vertex_positions.push(middle_point.to_point());
                self.library.b_vertex_path_ids.push(path_id);
                self.library
                    .b_vertex_loop_blinn_data
                    .push(BVertexLoopBlinnData::new(active_edge.endpoint_kind()));

                left_curve_control_point_vertex_index = u32::MAX;
            }
            _ => {
                let left_endpoint_position =
                    self.library.b_vertex_positions[active_edge.left_vertex_index as usize];
                let control_point_position =
                    self.library
                        .b_vertex_positions[active_edge.control_point_vertex_index as usize];
                let right_endpoint_position =
                    self.endpoints[active_edge.right_endpoint_index as usize].position;
                let original_curve = Curve::new(&left_endpoint_position,
                                                &control_point_position,
                                                &right_endpoint_position);
                let (left_curve, right_curve) = original_curve.subdivide(t);

                left_curve_control_point_vertex_index =
                    self.library.b_vertex_loop_blinn_data.len() as u32;
                active_edge.left_vertex_index = left_curve_control_point_vertex_index + 1;
                active_edge.control_point_vertex_index = left_curve_control_point_vertex_index + 2;

                self.library.b_vertex_positions.extend([
                    left_curve.control_point,
                    left_curve.endpoints[1],
                    right_curve.control_point,
                ].into_iter());
                self.library.b_vertex_path_ids.extend(iter::repeat(self.path_id).take(3));
                self.library.b_vertex_loop_blinn_data.extend([
                    BVertexLoopBlinnData::control_point(&left_curve.endpoints[0],
                                                        &left_curve.control_point,
                                                        &left_curve.endpoints[1],
                                                        bottom),
                    BVertexLoopBlinnData::new(active_edge.endpoint_kind()),
                    BVertexLoopBlinnData::control_point(&right_curve.endpoints[0],
                                                        &right_curve.control_point,
                                                        &right_curve.endpoints[1],
                                                        bottom),
                ].into_iter());
            }
        }

        SubdividedActiveEdge {
            left_curve_left: left_curve_left,
            left_curve_control_point: left_curve_control_point_vertex_index,
            middle_point: active_edge.left_vertex_index,
        }
    }

    fn prev_endpoint_of(&self, endpoint_index: u32) -> u32 {
        let endpoint = &self.endpoints[endpoint_index as usize];
        let subpath = &self.subpaths[endpoint.subpath_index as usize];
        if endpoint_index > subpath.first_endpoint_index {
            endpoint_index - 1
        } else {
            subpath.last_endpoint_index - 1
        }
    }

    fn next_endpoint_of(&self, endpoint_index: u32) -> u32 {
        let endpoint = &self.endpoints[endpoint_index as usize];
        let subpath = &self.subpaths[endpoint.subpath_index as usize];
        if endpoint_index + 1 < subpath.last_endpoint_index {
            endpoint_index + 1
        } else {
            subpath.first_endpoint_index
        }
    }

    fn create_point_from_endpoint(&self, endpoint_index: u32) -> Point {
        Point {
            position: self.endpoints[endpoint_index as usize].position,
            endpoint_index: endpoint_index,
        }
    }

    fn control_point_index_before_endpoint(&self, endpoint_index: u32) -> u32 {
        self.endpoints[endpoint_index as usize].control_point_index
    }

    fn control_point_index_after_endpoint(&self, endpoint_index: u32) -> u32 {
        self.control_point_index_before_endpoint(self.next_endpoint_of(endpoint_index))
    }
}

#[derive(Debug, Clone, Copy)]
struct Point {
    position: Point2D<f32>,
    endpoint_index: u32,
}

impl PartialEq for Point {
    #[inline]
    fn eq(&self, other: &Point) -> bool {
        self.position == other.position && self.endpoint_index == other.endpoint_index
    }
    #[inline]
    fn ne(&self, other: &Point) -> bool {
        self.position != other.position || self.endpoint_index != other.endpoint_index
    }
}

impl Eq for Point {}

impl PartialOrd for Point {
    #[inline]
    fn partial_cmp(&self, other: &Point) -> Option<Ordering> {
        // Reverse, because `std::collections::BinaryHeap` is a *max*-heap!
        match other.position.x.partial_cmp(&self.position.x) {
            None | Some(Ordering::Equal) => {}
            Some(ordering) => return Some(ordering),
        }
        match other.position.y.partial_cmp(&self.position.y) {
            None | Some(Ordering::Equal) => {}
            Some(ordering) => return Some(ordering),
        }
        other.endpoint_index.partial_cmp(&self.endpoint_index)
    }
}

impl Ord for Point {
    #[inline]
    fn cmp(&self, other: &Point) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveEdge {
    left_vertex_index: u32,
    control_point_vertex_index: u32,
    right_endpoint_index: u32,
    left_to_right: bool,
    parity: bool,
}

impl Default for ActiveEdge {
    fn default() -> ActiveEdge {
        ActiveEdge {
            left_vertex_index: 0,
            control_point_vertex_index: u32::MAX,
            right_endpoint_index: 0,
            left_to_right: false,
            parity: false,
        }
    }
}

impl ActiveEdge {
    fn toggle_parity(&mut self) {
        self.parity = !self.parity
    }

    fn endpoint_kind(&self) -> BVertexKind {
        if !self.parity {
            BVertexKind::Endpoint0
        } else {
            BVertexKind::Endpoint1
        }
    }

    #[inline]
    fn winding_number(&self) -> i32 {
        if self.left_to_right {
            1
        } else {
            -1
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SubdividedActiveEdge {
    left_curve_left: u32,
    left_curve_control_point: u32,
    middle_point: u32,
}

impl SubdividedActiveEdge {
    fn shape(&self, b_vertex_loop_blinn_data: &[BVertexLoopBlinnData]) -> Shape {
        if self.left_curve_control_point == u32::MAX {
            Shape::Flat
        } else if b_vertex_loop_blinn_data[self.left_curve_control_point as usize].sign < 0 {
            Shape::Convex
        } else {
            Shape::Concave
        }
    }

    fn to_curve(&self, b_vertex_positions: &[Point2D<f32>]) -> Option<Curve> {
        if self.left_curve_control_point == u32::MAX {
            None
        } else {
            Some(Curve::new(&b_vertex_positions[self.left_curve_left as usize],
                            &b_vertex_positions[self.left_curve_control_point as usize],
                            &b_vertex_positions[self.middle_point as usize]))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum EndpointClass {
    Min,
    Regular,
    Max,
}

#[derive(Debug, Clone, Copy)]
struct MatchingActiveEdges {
    indices: [u32; 2],
    count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    Flat,
    Convex,
    Concave,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BQuadEmissionResult {
    NoBQuadEmitted,
    BQuadEmittedBelow,
    BQuadEmittedAbove,
    BQuadEmittedAround,
}

impl BQuadEmissionResult {
    fn new(active_edge_index: u32, upper_active_edge_index: u32, lower_active_edge_index: u32)
           -> BQuadEmissionResult {
        if upper_active_edge_index == lower_active_edge_index {
            BQuadEmissionResult::NoBQuadEmitted
        } else if upper_active_edge_index == active_edge_index {
            BQuadEmissionResult::BQuadEmittedBelow
        } else if lower_active_edge_index == active_edge_index {
            BQuadEmissionResult::BQuadEmittedAbove
        } else {
            BQuadEmissionResult::BQuadEmittedAround
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SubdivisionType {
    Upper,
    Inside,
    Lower,
}
