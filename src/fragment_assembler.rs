/*
 * Copyright 2020 UT OVERSEAS INC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::HashMap;

use crate::buffer_builder::BufferBuilder;
use crate::concurrent::atomic_buffer::AtomicBuffer;
use crate::concurrent::logbuffer::data_frame_header;
use crate::concurrent::logbuffer::frame_descriptor;
use crate::concurrent::logbuffer::header::Header;
use crate::utils::errors::AeronError;
use crate::utils::types::Index;

const DEFAULT_FRAGMENT_ASSEMBLY_BUFFER_LENGTH: isize = 4096;

pub(crate) trait Fragment: FnMut(&AtomicBuffer, Index, Index, &Header) -> Result<(), AeronError> {}

impl<T: FnMut(&AtomicBuffer, Index, Index, &Header) -> Result<(), AeronError>> Fragment for T {}

/**
 * A handler that sits in a chain-of-responsibility pattern that reassembles fragmented messages
 * so that the next handler in the chain only sees whole messages.
 * <p>
 * Unfragmented messages are delegated without copy. Fragmented messages are copied to a temporary
 * buffer for reassembly before delegation.
 * <p>
 * The Header passed to the delegate on assembling a message will be that of the last fragment.
 * <p>
 * Session based buffers will be allocated and grown as necessary based on the length of messages to be assembled.
 * When sessions go inactive see {@link on_unavailable_image_t}, it is possible to free the buffer by calling
 * {@link #deleteSessionBuffer(std::int32_t)}.
 */
struct FragmentAssembler {
    delegate: Box<dyn Fragment>,
    builder_by_session_id_map: HashMap<i32, BufferBuilder>,
    initial_buffer_length: isize,
}

impl FragmentAssembler {
    /**
     * Construct an adapter to reassembly message fragments and delegate on only whole messages.
     *
     * @param delegate            onto which whole messages are forwarded.
     * @param initialBufferLength to be used for each session.
     */
    pub fn new(delegate: Box<dyn Fragment>, initial_buffer_length: Option<isize>) -> Self {
        Self {
            delegate,
            builder_by_session_id_map: HashMap::new(),
            initial_buffer_length: initial_buffer_length.unwrap_or(DEFAULT_FRAGMENT_ASSEMBLY_BUFFER_LENGTH),
        }
    }

    /**
     * Compose a fragment_handler_t that calls the this FragmentAssembler instance for reassembly. Suitable for
     * passing to Subscription::poll(fragment_handler_t, int).
     *
     * @return fragment_handler_t composed with the FragmentAssembler instance
     */
    pub fn handler(&mut self) -> impl Fragment + '_ {
        move |buffer: &AtomicBuffer, offset, length, header: &Header| self.on_fragment(buffer, offset, length, header)
    }

    /**
     * Free an existing session buffer to reduce memory pressure when an Image goes inactive or no more
     * large messages are expected.
     *
     * @param sessionId to have its buffer freed
     */
    pub fn delete_session_buffer(&mut self, session_id: i32) {
        self.builder_by_session_id_map.remove(&session_id);
    }

    #[inline]
    fn on_fragment(&mut self, buffer: &AtomicBuffer, offset: Index, length: Index, header: &Header) -> Result<(), AeronError> {
        let flags = header.flags();
        if (flags & frame_descriptor::UNFRAGMENTED) == frame_descriptor::UNFRAGMENTED {
            (*self.delegate)(buffer, offset, length, header)?;
        } else if (flags & frame_descriptor::BEGIN_FRAG) == frame_descriptor::BEGIN_FRAG {
            // FIXME: Check the logic to imitate C++ emplace
            let result = self
                .builder_by_session_id_map
                .insert(header.session_id(), BufferBuilder::new(self.initial_buffer_length));
            let mut builder = result.unwrap();
            builder.reset().append(buffer, offset, length, header)?;
        } else if let Some(builder) = self.builder_by_session_id_map.get_mut(&header.session_id()) {
            if builder.limit() != data_frame_header::LENGTH {
                builder.append(buffer, offset, length, header)?;
                if flags & frame_descriptor::END_FRAG == frame_descriptor::END_FRAG {
                    let msg_length = builder.limit() - data_frame_header::LENGTH;
                    let msg_buffer = AtomicBuffer::new(builder.buffer(), builder.limit());

                    (*self.delegate)(&msg_buffer, data_frame_header::LENGTH, msg_length, header)?;

                    builder.reset();
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use lazy_static::lazy_static;

    use crate::concurrent::atomic_buffer::{AlignedBuffer, AtomicBuffer};
    use crate::concurrent::logbuffer::data_frame_header::{self, DataFrameHeaderDefn};
    use crate::concurrent::logbuffer::header::Header;
    use crate::concurrent::logbuffer::{frame_descriptor, log_buffer_descriptor};
    use crate::fragment_assembler::FragmentAssembler;
    use crate::utils::{bit_utils, types::Index};

    const CHANNEL: &str = "aeron:udp?endpoint=localhost:40123";
    const STREAM_ID: i32 = 10;
    const SESSION_ID: i32 = 200;
    const TERM_LENGTH: i32 = log_buffer_descriptor::TERM_MIN_LENGTH;
    const INITIAL_TERM_ID: i32 = -1234;
    const ACTIVE_TERM_ID: i32 = INITIAL_TERM_ID + 5;
    const MTU_LENGTH: Index = 128;

    lazy_static! {
        pub static ref POSITION_BITS_TO_SHIFT: i32 = bit_utils::number_of_trailing_zeroes(TERM_LENGTH);
    }

    struct FragmentAssemblerTest {
        fragment: AlignedBuffer,
        buffer: AtomicBuffer,
        header: Header,
    }

    impl FragmentAssemblerTest {
        pub fn new() -> Self {
            let fragment = AlignedBuffer::with_capacity(TERM_LENGTH);
            let buffer = AtomicBuffer::from_aligned(&fragment);
            let mut header = Header::new(INITIAL_TERM_ID, TERM_LENGTH);
            header.set_buffer(buffer);
            Self {
                fragment,
                buffer,
                header,
            }
        }

        fn fill_frame(&self, flags: u8, offset: i32, length: i32, initial_payload_value: u8) {
            let frame = self.buffer.overlay_struct::<DataFrameHeaderDefn>(offset);
            unsafe {
                let mut frame = *frame;
                frame.frame_length = data_frame_header::LENGTH + length;
                frame.version = data_frame_header::CURRENT_VERSION;
                frame.flags = flags;
                frame.frame_type = data_frame_header::HDR_TYPE_DATA;
                frame.term_offset = offset;
                frame.session_id = SESSION_ID;
                frame.stream_id = STREAM_ID;
                frame.term_id = ACTIVE_TERM_ID;
            }
            let mut value = initial_payload_value;
            for i in 0..length {
                self.buffer.put(i + offset + data_frame_header::LENGTH, value);
                value += 1;
            }
        }

        fn verify_payload(buffer: &AtomicBuffer, offset: Index, length: Index) {
            unsafe {
                let ptr = buffer.buffer().offset(offset as isize);
                for i in 0..length {
                    assert_eq!(*(ptr.offset(i as isize)) as i32, i % 256);
                }
            }
        }
    }

    #[test]
    fn should_pass_through_unfragmented_message() {
        let test = FragmentAssemblerTest::new();
        let msg_length = 158;
        test.fill_frame(frame_descriptor::UNFRAGMENTED, 0, msg_length, 0);
        let mut called = false;
        let fragment = move |buffer: &AtomicBuffer, offset: Index, length: Index, header: &Header| {
            called = true;
            assert_eq!(offset, data_frame_header::LENGTH);
            assert_eq!(length, msg_length);
            assert_eq!(header.session_id(), SESSION_ID);
            assert_eq!(header.stream_id(), STREAM_ID);
            assert_eq!(header.term_id(), ACTIVE_TERM_ID);
            assert_eq!(header.term_offset(), 0);
            assert_eq!(header.frame_length(), data_frame_header::LENGTH + msg_length);
            assert_eq!(header.flags(), frame_descriptor::UNFRAGMENTED);
            assert_eq!(
                header.position(),
                log_buffer_descriptor::compute_position(
                    ACTIVE_TERM_ID,
                    bit_utils::align(
                        header.term_offset() + header.frame_length(),
                        frame_descriptor::FRAME_ALIGNMENT
                    ),
                    *POSITION_BITS_TO_SHIFT,
                    INITIAL_TERM_ID
                )
            );
            FragmentAssemblerTest::verify_payload(buffer, offset, length);
            Ok(())
        };

        let mut adapter = FragmentAssembler::new(Box::new(fragment), None);

        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(called);
    }

    #[test]
    fn should_reassemble_from_two_fragments() {
        let mut test = FragmentAssemblerTest::new();
        let msg_length = MTU_LENGTH - data_frame_header::LENGTH;
        let mut called = false;

        let fragment = move |buffer: &AtomicBuffer, offset: Index, length: Index, header: &Header| {
            called = true;
            assert_eq!(offset, data_frame_header::LENGTH);
            assert_eq!(length, msg_length * 2);
            assert_eq!(header.session_id(), SESSION_ID);
            assert_eq!(header.stream_id(), STREAM_ID);
            assert_eq!(header.term_id(), ACTIVE_TERM_ID);
            assert_eq!(header.term_offset(), 0);
            assert_eq!(header.frame_length(), data_frame_header::LENGTH + msg_length);
            assert_eq!(header.flags(), frame_descriptor::END_FRAG);
            assert_eq!(
                header.position(),
                log_buffer_descriptor::compute_position(
                    ACTIVE_TERM_ID,
                    bit_utils::align(
                        header.term_offset() + header.frame_length(),
                        frame_descriptor::FRAME_ALIGNMENT
                    ),
                    *POSITION_BITS_TO_SHIFT,
                    INITIAL_TERM_ID
                )
            );
            FragmentAssemblerTest::verify_payload(buffer, offset, length);
            Ok(())
        };

        let mut adapter = FragmentAssembler::new(Box::new(fragment), None);

        test.fill_frame(frame_descriptor::BEGIN_FRAG, 0, msg_length, 0);
        test.header.set_offset(0);
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(!called);

        test.header.set_offset(MTU_LENGTH);
        test.fill_frame(frame_descriptor::END_FRAG, MTU_LENGTH, msg_length, (msg_length % 256) as u8);
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(called);
    }

    #[test]
    fn should_reassemble_from_three_fragments() {
        let mut test = FragmentAssemblerTest::new();
        let msg_length = MTU_LENGTH - data_frame_header::LENGTH;
        let mut called = false;

        let fragment = move |buffer: &AtomicBuffer, offset: Index, length: Index, header: &Header| {
            called = true;
            assert_eq!(offset, data_frame_header::LENGTH);
            assert_eq!(length, msg_length * 3);
            assert_eq!(header.session_id(), SESSION_ID);
            assert_eq!(header.stream_id(), STREAM_ID);
            assert_eq!(header.term_id(), ACTIVE_TERM_ID);
            assert_eq!(header.term_offset(), 0);
            assert_eq!(header.frame_length(), data_frame_header::LENGTH + msg_length);
            assert_eq!(header.flags(), frame_descriptor::END_FRAG);
            assert_eq!(
                header.position(),
                log_buffer_descriptor::compute_position(
                    ACTIVE_TERM_ID,
                    bit_utils::align(
                        header.term_offset() + header.frame_length(),
                        frame_descriptor::FRAME_ALIGNMENT
                    ),
                    *POSITION_BITS_TO_SHIFT,
                    INITIAL_TERM_ID
                )
            );
            FragmentAssemblerTest::verify_payload(buffer, offset, length);
            Ok(())
        };

        let mut adapter = FragmentAssembler::new(Box::new(fragment), None);

        test.fill_frame(frame_descriptor::BEGIN_FRAG, 0, msg_length, 0);
        test.header.set_offset(0);
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(!called);

        test.header.set_offset(MTU_LENGTH);
        test.fill_frame(frame_descriptor::END_FRAG, MTU_LENGTH, msg_length, (msg_length % 256) as u8);
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(!called);

        test.header.set_offset(MTU_LENGTH * 2);
        test.fill_frame(
            frame_descriptor::END_FRAG,
            MTU_LENGTH * 2,
            msg_length,
            ((msg_length * 2) % 256) as u8,
        );
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(called);
    }

    #[test]
    fn should_not_reassemble_if_end_first_fragment() {
        let mut test = FragmentAssemblerTest::new();
        let msg_length = MTU_LENGTH - data_frame_header::LENGTH;
        let mut called = false;

        let fragment = move |_buffer: &AtomicBuffer, _offset: Index, _length: Index, _header: &Header| {
            called = true;
            Ok(())
        };

        let mut adapter = FragmentAssembler::new(Box::new(fragment), None);

        test.header.set_offset(MTU_LENGTH);
        test.fill_frame(frame_descriptor::END_FRAG, MTU_LENGTH, msg_length, (msg_length % 256) as u8);
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(!called);
    }

    #[test]
    fn should_not_reassemble_if_missing_begin() {
        let mut test = FragmentAssemblerTest::new();
        let msg_length = MTU_LENGTH - data_frame_header::LENGTH;
        let mut called = false;

        let fragment = move |_buffer: &AtomicBuffer, _offset: Index, _length: Index, _header: &Header| {
            called = true;
            Ok(())
        };

        let mut adapter = FragmentAssembler::new(Box::new(fragment), None);

        test.header.set_offset(MTU_LENGTH);
        test.fill_frame(frame_descriptor::END_FRAG, MTU_LENGTH, msg_length, (msg_length % 256) as u8);
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(!called);

        test.header.set_offset(MTU_LENGTH * 2);
        test.fill_frame(
            frame_descriptor::END_FRAG,
            MTU_LENGTH * 2,
            msg_length,
            ((msg_length * 2) % 256) as u8,
        );
        adapter.handler()(&test.buffer, data_frame_header::LENGTH, msg_length, &test.header).unwrap();
        assert!(!called);
    }
}
