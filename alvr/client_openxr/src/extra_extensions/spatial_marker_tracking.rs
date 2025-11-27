use alvr_common::RelaxedAtomic;
use openxr::{
    self as xr, AsHandle, raw,
    sys::{self, Handle},
};
use std::{
    ffi::{CStr, c_char},
    ptr, thread,
    time::{Duration, Instant},
};

const CAPABILITY: sys::SpatialCapabilityEXT = sys::SpatialCapabilityEXT::MARKER_TRACKING_QR_CODE;
// Note: The Meta implementation is currently bugged and the buffer capacity cannot be changed once
// the fist call of query_spatial_component_data is made.
const MAX_MARKERS_COUNT: usize = 32;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(1);

pub struct QRCodesSpatialContext {
    instance: xr::Instance,
    spatial_entity_fns: raw::SpatialEntityEXT,
    inner: sys::SpatialContextEXT,
    enabled_components: Vec<sys::SpatialComponentTypeEXT>,
    entity_ids: [sys::SpatialEntityIdEXT; MAX_MARKERS_COUNT],
    entity_states: [sys::SpatialEntityTrackingStateEXT; MAX_MARKERS_COUNT],
    bounded_2d_arr: [sys::SpatialBounded2DDataEXT; MAX_MARKERS_COUNT],
    marker_arr: [sys::SpatialMarkerDataEXT; MAX_MARKERS_COUNT],
    string_buffer: [c_char; 256],
    discovery_enabled: RelaxedAtomic,
    discovery_timeout_deadline: Instant,
    snapshot_future: Option<sys::FutureEXT>,
}

impl QRCodesSpatialContext {
    pub fn new<G>(session: &xr::Session<G>, initial_discovery_state: bool) -> xr::Result<Self> {
        let spatial_entity_fns = session
            .instance()
            .exts()
            .ext_spatial_entity
            .ok_or(sys::Result::ERROR_EXTENSION_NOT_PRESENT)?;
        if session
            .instance()
            .exts()
            .ext_spatial_marker_tracking
            .is_none()
        {
            return Err(sys::Result::ERROR_EXTENSION_NOT_PRESENT);
        }
        alvr_common::error!("MarkerSpatialContext 1");

        let enabled_components = vec![
            sys::SpatialComponentTypeEXT::BOUNDED_2D,
            sys::SpatialComponentTypeEXT::MARKER,
        ];

        let qr_code_capability_configuration = sys::SpatialCapabilityConfigurationQrCodeEXT {
            ty: sys::SpatialCapabilityConfigurationQrCodeEXT::TYPE,
            next: ptr::null(),
            capability: CAPABILITY,
            enabled_component_count: enabled_components.len() as u32,
            enabled_components: enabled_components.as_ptr(),
        };

        let base_capability_configuration = ptr::from_ref(&qr_code_capability_configuration).cast();
        let spatial_context_create_info = sys::SpatialContextCreateInfoEXT {
            ty: sys::SpatialContextCreateInfoEXT::TYPE,
            next: ptr::null(),
            capability_config_count: 1,
            capability_configs: &raw const base_capability_configuration,
        };

        let mut create_context_future: sys::FutureEXT = 0;
        unsafe {
            super::xr_res((spatial_entity_fns.create_spatial_context_async)(
                session.as_handle(),
                &spatial_context_create_info,
                &mut create_context_future,
            ))?;
        }

        loop {
            if super::check_future(session.instance(), create_context_future)? {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }

        let completion = unsafe {
            let mut completion = sys::CreateSpatialContextCompletionEXT::out(ptr::null_mut());
            super::xr_res((spatial_entity_fns.create_spatial_context_complete)(
                session.as_handle(),
                create_context_future,
                completion.as_mut_ptr(),
            ))?;
            completion.assume_init()
        };
        if completion.future_result != sys::Result::SUCCESS {
            return Err(completion.future_result);
        }

        Ok(QRCodesSpatialContext {
            instance: session.instance().clone(),
            spatial_entity_fns,
            inner: completion.spatial_context,
            enabled_components,
            entity_ids: [xr::sys::SpatialEntityIdEXT::default(); MAX_MARKERS_COUNT],
            entity_states: [xr::sys::SpatialEntityTrackingStateEXT::STOPPED; MAX_MARKERS_COUNT],
            bounded_2d_arr: [xr::sys::SpatialBounded2DDataEXT::default(); MAX_MARKERS_COUNT],
            marker_arr: [sys::SpatialMarkerDataEXT {
                capability: CAPABILITY,
                marker_id: 0,
                data: sys::SpatialBufferEXT {
                    buffer_id: sys::SpatialBufferIdEXT::NULL,
                    buffer_type: sys::SpatialBufferTypeEXT::UNKNOWN,
                },
            }; MAX_MARKERS_COUNT],
            string_buffer: [0; 256],
            discovery_enabled: RelaxedAtomic::new(initial_discovery_state),
            discovery_timeout_deadline: Instant::now(),
            snapshot_future: None,
        })
    }

    pub fn set_discovery_enabled(&self, enabled: bool) {
        self.discovery_enabled.set(enabled);
    }

    pub fn poll(
        &mut self,
        base_space: &xr::Space,
        time: xr::Time,
    ) -> xr::Result<Option<Vec<(String, xr::Posef)>>> {
        let new = Instant::now();

        if self.snapshot_future.is_none()
            && self.discovery_enabled.value()
            && new > self.discovery_timeout_deadline
        {
            let snapshot_create_info = sys::SpatialDiscoverySnapshotCreateInfoEXT {
                ty: sys::SpatialDiscoverySnapshotCreateInfoEXT::TYPE,
                next: ptr::null(),
                component_type_count: self.enabled_components.len() as u32,
                component_types: self.enabled_components.as_ptr(),
            };

            let mut create_snapshot_future: sys::FutureEXT = 0;
            unsafe {
                super::xr_res((self
                    .spatial_entity_fns
                    .create_spatial_discovery_snapshot_async)(
                    self.inner,
                    &snapshot_create_info,
                    &mut create_snapshot_future,
                ))?;
            }

            self.snapshot_future = Some(create_snapshot_future);
            self.discovery_timeout_deadline = new + DISCOVERY_TIMEOUT;
        };

        let &Some(future) = &self.snapshot_future else {
            return Ok(None);
        };

        // Return if the future is not completed
        if !super::check_future(&self.instance, future)? {
            return Ok(None);
        }

        self.snapshot_future = None;

        let completion_info = sys::CreateSpatialDiscoverySnapshotCompletionInfoEXT {
            ty: sys::CreateSpatialDiscoverySnapshotCompletionInfoEXT::TYPE,
            next: ptr::null(),
            base_space: base_space.as_handle(),
            time,
            future,
        };

        let completion = unsafe {
            let mut completion =
                sys::CreateSpatialDiscoverySnapshotCompletionEXT::out(ptr::null_mut());
            super::xr_res((self
                .spatial_entity_fns
                .create_spatial_discovery_snapshot_complete)(
                self.inner,
                &completion_info,
                completion.as_mut_ptr(),
            ))?;
            completion.assume_init()
        };
        if completion.future_result != sys::Result::SUCCESS {
            return Err(completion.future_result);
        }

        let query_contition = sys::SpatialComponentDataQueryConditionEXT {
            ty: sys::SpatialComponentDataQueryConditionEXT::TYPE,
            next: ptr::null(),
            component_type_count: self.enabled_components.len() as u32,
            component_types: self.enabled_components.as_ptr(),
        };

        let mut query_result = sys::SpatialComponentDataQueryResultEXT {
            ty: sys::SpatialComponentDataQueryResultEXT::TYPE,
            next: ptr::null_mut(),
            entity_id_capacity_input: MAX_MARKERS_COUNT as u32,
            entity_id_count_output: 0,
            entity_ids: self.entity_ids.as_mut_ptr(),
            entity_state_capacity_input: MAX_MARKERS_COUNT as u32,
            entity_state_count_output: 0,
            entity_states: self.entity_states.as_mut_ptr(),
        };
        unsafe {
            super::xr_res((self.spatial_entity_fns.query_spatial_component_data)(
                completion.snapshot,
                &query_contition,
                &mut query_result,
            ))?;
        }
        let marker_count = query_result.entity_id_count_output;

        let mut bounded_2d_list = sys::SpatialComponentBounded2DListEXT {
            ty: sys::SpatialComponentBounded2DListEXT::TYPE,
            next: ptr::null_mut(),
            bound_count: marker_count,
            bounds: self.bounded_2d_arr.as_mut_ptr(),
        };
        query_result.next = (&raw mut bounded_2d_list).cast();

        let mut marker_list = sys::SpatialComponentMarkerListEXT {
            ty: sys::SpatialComponentMarkerListEXT::TYPE,
            next: ptr::null_mut(),
            marker_count,
            markers: self.marker_arr.as_mut_ptr(),
        };
        bounded_2d_list.next = (&raw mut marker_list).cast();

        unsafe {
            super::xr_res((self.spatial_entity_fns.query_spatial_component_data)(
                completion.snapshot,
                &query_contition,
                &mut query_result,
            ))?;
        }

        let mut out_markers = vec![];
        for idx in 0..query_result.entity_id_count_output as usize {
            if self.entity_states[idx] != sys::SpatialEntityTrackingStateEXT::TRACKING
                || self.marker_arr[idx].capability != CAPABILITY
                || self.marker_arr[idx].data.buffer_id == sys::SpatialBufferIdEXT::NULL
                || self.marker_arr[idx].data.buffer_type != sys::SpatialBufferTypeEXT::STRING
            {
                alvr_common::debug!(
                    "Parsing marker failed! {:?} {:?} {:?} {:?}",
                    self.entity_states[idx],
                    self.marker_arr[idx].capability,
                    self.marker_arr[idx].data.buffer_id,
                    self.marker_arr[idx].data.buffer_type
                );
                continue;
            }

            let get_info = sys::SpatialBufferGetInfoEXT {
                ty: sys::SpatialBufferGetInfoEXT::TYPE,
                next: ptr::null(),
                buffer_id: self.marker_arr[idx].data.buffer_id,
            };

            let string = unsafe {
                let mut _string_lenght = 0;
                super::xr_res((self.spatial_entity_fns.get_spatial_buffer_string)(
                    completion.snapshot,
                    &get_info,
                    self.string_buffer.len() as u32,
                    &mut _string_lenght,
                    self.string_buffer.as_mut_ptr(),
                ))?;

                CStr::from_ptr(self.string_buffer.as_ptr())
                    .to_str()
                    .map_err(|_| sys::Result::ERROR_SPATIAL_BUFFER_ID_INVALID_EXT)?
                    .to_owned()
            };

            let pose = self.bounded_2d_arr[idx].center;

            out_markers.push((string, pose));
        }

        Ok(Some(out_markers))
    }
}

impl Drop for QRCodesSpatialContext {
    fn drop(&mut self) {
        unsafe {
            (self.spatial_entity_fns.destroy_spatial_context)(self.inner);
        }
    }
}
