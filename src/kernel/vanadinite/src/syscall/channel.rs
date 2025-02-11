// SPDX-License-Identifier: MPL-2.0
// SPDX-FileCopyrightText: 2021 The vanadinite developers
//
// This Source Code Form is subject to the terms of the Mozilla Public License,
// v. 2.0. If a copy of the MPL was not distributed with this file, You can
// obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    mem::{
        manager::{AddressRegionKind, FillOption, RegionDescription},
        paging::{flags, PageSize, VirtualAddress},
        region::{MemoryRegion, PhysicalRegion},
    },
    scheduler::{CURRENT_TASK, TASKS},
    task::{Task, TaskState},
    utils::{self, Units},
};
use alloc::{collections::BTreeMap, sync::Arc};
use core::{
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};
use librust::{
    error::KError,
    message::{KernelNotification, Message, Sender, SyscallResult},
    syscalls::channel::{ChannelId, MessageId},
    task::Tid,
};

pub const MAX_CHANNEL_BYTES: usize = 4096;

pub struct UserspaceChannel {
    other_task: Tid,
    other_channel_id: ChannelId,
    message_id_counter: Arc<AtomicUsize>,
    write_regions: BTreeMap<MessageId, Range<VirtualAddress>>,
    read_regions: BTreeMap<MessageId, (Range<VirtualAddress>, usize)>,
}

impl UserspaceChannel {
    fn next_message_id(&self) -> usize {
        self.message_id_counter.fetch_add(1, Ordering::AcqRel)
    }
}

pub fn request_channel(from: &mut Task, to: Tid) -> SyscallResult<Message, KError> {
    let current_tid = CURRENT_TASK.get().unwrap();

    // Doesn't make sense to make a shared memory channel with itself and we'd
    // also end up deadlocking ourselves
    if current_tid == to {
        return SyscallResult::Err(KError::InvalidArgument(0));
    }

    let to_task = match TASKS.get(to) {
        Some(task) => task,
        None => return SyscallResult::Err(KError::InvalidRecipient),
    };

    let mut to_task = to_task.lock();

    if to_task.state.is_dead() {
        return SyscallResult::Err(KError::InvalidRecipient);
    } else if !to_task.promiscuous {
        return SyscallResult::Ok(KernelNotification::ChannelRequestDenied.into());
    }

    to_task.incoming_channel_request.insert(current_tid);
    to_task.message_queue.push_back((Sender::kernel(), KernelNotification::ChannelRequest(current_tid).into()));

    log::info!("blocking {:?}", current_tid);
    from.state = TaskState::Blocked;

    SyscallResult::Ok(Message::default())
}

pub fn create_channel(from: &mut Task, to: Tid) -> SyscallResult<usize, KError> {
    let current_tid = CURRENT_TASK.get().unwrap();

    // Doesn't make sense to make a shared memory channel with itself and we'd
    // also end up deadlocking ourselves
    if current_tid == to {
        return SyscallResult::Err(KError::InvalidArgument(0));
    }

    let to_task = match TASKS.get(to) {
        Some(task) => task,
        None => return SyscallResult::Err(KError::InvalidRecipient),
    };

    let mut to_task = to_task.lock();

    if to_task.state.is_dead() {
        return SyscallResult::Err(KError::InvalidRecipient);
    }

    let counter = Arc::new(AtomicUsize::new(0));

    let from_channel_id = ChannelId::new(from.channels.last_key_value().map(|(id, _)| id.value() + 1).unwrap_or(0));
    let to_channel_id = ChannelId::new(to_task.channels.last_key_value().map(|(id, _)| id.value() + 1).unwrap_or(0));

    let from_channel = UserspaceChannel {
        other_task: to,
        other_channel_id: to_channel_id,
        message_id_counter: counter.clone(),
        write_regions: BTreeMap::new(),
        read_regions: BTreeMap::new(),
    };

    let to_channel = UserspaceChannel {
        other_task: current_tid,
        other_channel_id: from_channel_id,
        message_id_counter: counter,
        write_regions: BTreeMap::new(),
        read_regions: BTreeMap::new(),
    };

    if from.incoming_channel_request.remove(&to) {
        log::info!("unblocking {:?}", to);
        to_task.state = TaskState::Running;
    }

    from.channels.insert(from_channel_id, from_channel);
    to_task.channels.insert(to_channel_id, to_channel);

    to_task.message_queue.push_front((Sender::kernel(), KernelNotification::ChannelOpened(to_channel_id).into()));

    SyscallResult::Ok(from_channel_id.value())
}

// FIXME: Definitely should be a way to return tuple values that can be
// converted into `usize` so its a lot more clear what's what
pub fn create_message(task: &mut Task, channel_id: usize, size: usize) -> SyscallResult<(usize, usize, usize), KError> {
    let channel_id = ChannelId::new(channel_id);
    let channel = match task.channels.get_mut(&channel_id) {
        Some(channel) => channel,
        None => return SyscallResult::Err(KError::InvalidArgument(0)),
    };

    let n_pages = utils::round_up_to_next(size, 4.kib()) / 4.kib();

    let message_id = channel.next_message_id();
    let (region, _) = task.memory_manager.alloc_shared_region(
        None,
        RegionDescription {
            size: PageSize::Kilopage,
            len: n_pages,
            contiguous: false,
            flags: flags::READ | flags::WRITE | flags::USER | flags::VALID,
            fill: FillOption::Zeroed,
            kind: AddressRegionKind::Channel,
        },
    );

    let size = n_pages * 4.kib();

    channel.write_regions.insert(MessageId::new(message_id), region.clone());

    SyscallResult::Ok((message_id, region.start.as_usize(), size))
}

pub fn send_message(task: &mut Task, channel_id: usize, message_id: usize, len: usize) -> SyscallResult<(), KError> {
    let channel_id = ChannelId::new(channel_id);
    let channel = match task.channels.get_mut(&channel_id) {
        Some(channel) => channel,
        None => return SyscallResult::Err(KError::InvalidArgument(0)),
    };

    let range = match channel.write_regions.remove(&MessageId::new(message_id)) {
        Some(range) => range,
        None => return SyscallResult::Err(KError::InvalidArgument(1)),
    };

    if range.end.as_usize() - range.start.as_usize() < len {
        return SyscallResult::Err(KError::InvalidArgument(2));
    }

    let backing = match task.memory_manager.dealloc_region(range.start) {
        MemoryRegion::Backed(PhysicalRegion::Shared(phys_region)) => phys_region,
        _ => unreachable!(),
    };

    let other = TASKS.get(channel.other_task).unwrap();
    let mut other = other.lock();

    let region = other.memory_manager.apply_shared_region(
        None,
        flags::READ | flags::WRITE | flags::USER | flags::VALID,
        backing,
        AddressRegionKind::Channel,
    );

    let other_channel = other.channels.get_mut(&channel.other_channel_id).unwrap();
    other_channel.read_regions.insert(MessageId::new(message_id), (region, len));

    SyscallResult::Ok(())
}

pub fn read_message(task: &mut Task, channel_id: usize) -> SyscallResult<(usize, usize, usize), KError> {
    let id = ChannelId::new(channel_id);
    let channel = match task.channels.get_mut(&id) {
        Some(channel) => channel,
        None => return SyscallResult::Err(KError::InvalidArgument(0)),
    };

    // TODO: need to be able to return more than just the first one
    match channel.read_regions.iter().next() {
        Some((id, (region, len))) => SyscallResult::Ok((id.value(), region.start.as_usize(), *len)),
        None => SyscallResult::Ok((0, 0, 0)),
    }
}

pub fn retire_message(task: &mut Task, channel_id: usize, message_id: usize) -> SyscallResult<(), KError> {
    let id = ChannelId::new(channel_id);
    let channel = match task.channels.get_mut(&id) {
        Some(channel) => channel,
        None => return SyscallResult::Err(KError::InvalidArgument(0)),
    };

    match channel.read_regions.remove(&MessageId::new(message_id)) {
        Some(region) => {
            task.memory_manager.dealloc_region(region.0.start);
            SyscallResult::Ok(())
        }
        None => SyscallResult::Err(KError::InvalidArgument(1)),
    }
}
