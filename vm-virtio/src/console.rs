// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use epoll;
use libc::EFD_NONBLOCK;
use std;
use std::cmp;
use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::result;
use std::sync::{Arc, Mutex};
use std::thread;

use super::Error as DeviceError;
use super::{
    ActivateError, ActivateResult, DeviceEventT, Queue, VirtioDevice, VirtioDeviceType,
    VirtioInterruptType, VIRTIO_F_VERSION_1,
};
use crate::VirtioInterrupt;
use vm_memory::{Bytes, GuestMemoryMmap};
use vmm_sys_util::EventFd;

const QUEUE_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES];

// New descriptors are pending on the virtio queue.
const INPUT_QUEUE_EVENT: DeviceEventT = 0;
const OUTPUT_QUEUE_EVENT: DeviceEventT = 1;
// Some input from the VMM is ready to be injected into the VM.
const INPUT_EVENT: DeviceEventT = 2;
// The device has been dropped.
const KILL_EVENT: DeviceEventT = 3;

struct ConsoleEpollHandler {
    queues: Vec<Queue>,
    mem: GuestMemoryMmap,
    interrupt_cb: Arc<VirtioInterrupt>,
    in_buffer: Arc<Mutex<VecDeque<u8>>>,
    out: Box<io::Write + Send>,
    input_queue_evt: EventFd,
    output_queue_evt: EventFd,
    input_evt: EventFd,
    kill_evt: EventFd,
}

impl ConsoleEpollHandler {
    /*
     * Each port of virtio console device has one receive
     * queue. One or more empty buffers are placed by the
     * dirver in the receive queue for incoming data. Here,
     * we place the input data to these empty buffers.
     */
    fn process_input_queue(&mut self) -> bool {
        let mut in_buffer = self.in_buffer.lock().unwrap();
        let count = in_buffer.len();
        let recv_queue = &mut self.queues[0]; //receiveq
        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0;
        let mut write_count = 0;

        for avail_desc in recv_queue.iter(&self.mem) {
            let len;

            let limit = cmp::min(write_count + avail_desc.len as u32, count as u32);
            let source_slice = in_buffer
                .drain(write_count as usize..limit as usize)
                .collect::<Vec<u8>>();
            let write_result = self.mem.write_slice(&source_slice[..], avail_desc.addr);

            match write_result {
                Ok(_) => {
                    len = limit - write_count; //avail_desc.len;
                    write_count = limit;
                }
                Err(e) => {
                    error!("Failed to write slice: {:?}", e);
                    break;
                }
            }

            used_desc_heads[used_count] = (avail_desc.index, len);
            used_count += 1;

            if write_count >= count as u32 {
                break;
            }
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            recv_queue.add_used(&self.mem, desc_index, len);
        }
        used_count > 0
    }

    /*
     * Each port of virtio console device has one transmit
     * queue. For outgoing data, characters are placed in
     * the transmit queue by the driver. Therefore, here
     * we read data from the transmit queue and flush them
     * to the referenced address.
     */
    fn process_output_queue(&mut self) -> bool {
        let trans_queue = &mut self.queues[1]; //transmitq
        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0;

        for avail_desc in trans_queue.iter(&self.mem) {
            let len;
            let _ = self
                .mem
                .write_to(avail_desc.addr, &mut self.out, avail_desc.len as usize);
            let _ = self.out.flush();

            len = avail_desc.len;
            used_desc_heads[used_count] = (avail_desc.index, len);
            used_count += 1;
        }

        for &(desc_index, len) in &used_desc_heads[..used_count] {
            trans_queue.add_used(&self.mem, desc_index, len);
        }
        used_count > 0
    }

    fn signal_used_queue(&self) -> result::Result<(), DeviceError> {
        (self.interrupt_cb)(&VirtioInterruptType::Queue, Some(&self.queues[0])).map_err(|e| {
            error!("Failed to signal used queue: {:?}", e);
            DeviceError::FailedSignalingUsedQueue(e)
        })
    }

    fn run(&mut self) -> result::Result<(), DeviceError> {
        // Create the epoll file descriptor
        let epoll_fd = epoll::create(true).map_err(DeviceError::EpollCreateFd)?;

        // Add events
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.input_queue_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(INPUT_QUEUE_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.output_queue_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(OUTPUT_QUEUE_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.input_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(INPUT_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            self.kill_evt.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, u64::from(KILL_EVENT)),
        )
        .map_err(DeviceError::EpollCtl)?;

        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];

        'epoll: loop {
            let num_events = match epoll::wait(epoll_fd, -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(DeviceError::EpollWait(e));
                }
            };

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    INPUT_QUEUE_EVENT => {
                        if let Err(e) = self.input_queue_evt.read() {
                            error!("Failed to get queue event: {:?}", e);
                            break 'epoll;
                        }
                    }
                    OUTPUT_QUEUE_EVENT => {
                        if let Err(e) = self.output_queue_evt.read() {
                            error!("Failed to get queue event: {:?}", e);
                            break 'epoll;
                        } else {
                            self.process_output_queue();
                        }
                    }
                    INPUT_EVENT => {
                        if let Err(e) = self.input_evt.read() {
                            error!("Failed to get input event: {:?}", e);
                            break 'epoll;
                        } else if self.process_input_queue() {
                            if let Err(e) = self.signal_used_queue() {
                                error!("Failed to signal used queue: {:?}", e);
                                break 'epoll;
                            }
                        }
                    }
                    KILL_EVENT => {
                        debug!("KILL_EVENT received, stopping epoll loop");
                        break 'epoll;
                    }
                    _ => {
                        error!("Unknown event for virtio-console");
                    }
                }
            }
        }

        Ok(())
    }
}

/// Virtio device for exposing console to the guest OS through virtio.
pub struct Console {
    kill_evt: Option<EventFd>,
    avail_features: u64,
    acked_features: u64,
    input: Arc<ConsoleInput>,
    out: Option<Box<io::Write + Send>>,
}

/// Input device.
pub struct ConsoleInput {
    input_evt: EventFd,
    in_buffer: Arc<Mutex<VecDeque<u8>>>,
}

impl ConsoleInput {
    pub fn queue_input_bytes(&self, input: &[u8]) {
        let mut in_buffer = self.in_buffer.lock().unwrap();
        in_buffer.extend(input);
        let _ = self.input_evt.write(1);
    }
}

impl Console {
    /// Create a new virtio console device that gets random data from /dev/urandom.
    pub fn new(out: Option<Box<io::Write + Send>>) -> io::Result<(Console, Arc<ConsoleInput>)> {
        let avail_features = 1u64 << VIRTIO_F_VERSION_1;

        let input_evt = EventFd::new(EFD_NONBLOCK).unwrap();

        let console_input = Arc::new(ConsoleInput {
            input_evt,
            in_buffer: Arc::new(Mutex::new(VecDeque::new())),
        });

        Ok((
            Console {
                kill_evt: None,
                avail_features,
                acked_features: 0u64,
                input: console_input.clone(),
                out,
            },
            console_input,
        ))
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u32 {
        VirtioDeviceType::TYPE_CONSOLE as u32
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            // Get the lower 32-bits of the features bitfield.
            0 => self.avail_features as u32,
            // Get the upper 32-bits of the features bitfield.
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!("Received request for unknown features page.");
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => {
                warn!("Cannot acknowledge unknown features page.");
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature.");

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, _offset: u64, _data: &mut [u8]) {
        warn!("Device specific configuration is not defined yet");
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        warn!("Device specific configuration is not defined yet");
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt_cb: Arc<VirtioInterrupt>,
        queues: Vec<Queue>,
        mut queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let (self_kill_evt, kill_evt) =
            match EventFd::new(EFD_NONBLOCK).and_then(|e| Ok((e.try_clone()?, e))) {
                Ok(v) => v,
                Err(e) => {
                    error!("failed creating kill EventFd pair: {}", e);
                    return Err(ActivateError::BadActivate);
                }
            };
        self.kill_evt = Some(self_kill_evt);

        if let Some(out) = self.out.take() {
            let mut handler = ConsoleEpollHandler {
                queues,
                mem,
                interrupt_cb,
                in_buffer: self.input.in_buffer.clone(),
                out,
                input_queue_evt: queue_evts.remove(0),
                output_queue_evt: queue_evts.remove(0),
                input_evt: self.input.input_evt.try_clone().unwrap(),
                kill_evt,
            };

            let worker_result = thread::Builder::new()
                .name("virtio_console".to_string())
                .spawn(move || handler.run());

            if let Err(e) = worker_result {
                error!("failed to spawn virtio_console worker: {}", e);
                return Err(ActivateError::BadActivate);;
            }

            return Ok(());
        }
        Err(ActivateError::BadActivate)
    }
}