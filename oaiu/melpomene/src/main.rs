use std::{sync::atomic::{Ordering, compiler_fence, AtomicBool}, thread::{spawn, yield_now, sleep}, time::Duration, ops::Deref};

use abi::bbqueue_ipc::BBBuffer;

const RING_SIZE: usize = 4096;
const HEAP_SIZE: usize = 192 * 1024;
static KERNEL_LOCK: AtomicBool = AtomicBool::new(true);

fn main() {
    println!("========================================");
    let kernel = spawn(move || {
        kernel_entry();
    });
    println!("[Melpo]: Kernel started.");

    // Wait for the kernel to complete initialization...
    while KERNEL_LOCK.load(Ordering::Acquire) {
        yield_now();
    }

    let userspace = spawn(move || {
        userspace_entry();
    });
    println!("[Melpo]: Userspace started.");
    println!("========================================");

    let kj = kernel.join();
    sleep(Duration::from_millis(50));
    let uj = userspace.join();
    sleep(Duration::from_millis(50));

    println!("========================================");
    println!("[Melpo]: Kernel ended:    {:?}", kj);
    println!("[Melpo]: Userspace ended: {:?}", uj);
    println!("========================================");

    println!("[Melpo]: You've met with a terrible fate, haven't you?");
}

fn kernel_entry() {
    let u2k = Box::into_raw(Box::new(BBBuffer::new()));
    let u2k_buf = Box::into_raw(Box::new([0u8; RING_SIZE]));
    let k2u = Box::into_raw(Box::new(BBBuffer::new()));
    let k2u_buf = Box::into_raw(Box::new([0u8; RING_SIZE]));

    let user_heap = Box::into_raw(Box::new([0u8; HEAP_SIZE]));
    abi::U2K_RING.store(u2k, Ordering::Relaxed);
    abi::K2U_RING.store(k2u, Ordering::Relaxed);
    abi::HEAP_PTR.store(user_heap.cast(), Ordering::Relaxed);
    abi::HEAP_LEN.store(HEAP_SIZE, Ordering::Relaxed);

    // TODO: The kernel is supposed to do this...
    unsafe {
        (*u2k).initialize(u2k_buf.cast(), RING_SIZE);
        (*k2u).initialize(k2u_buf.cast(), RING_SIZE);
    }

    let u2k = unsafe { BBBuffer::take_framed_consumer(u2k) };
    let _k2u = unsafe { BBBuffer::take_framed_producer(k2u) };

    println!("DING!");
    compiler_fence(Ordering::SeqCst);

    loop {
        while !KERNEL_LOCK.load(Ordering::Acquire) {
            yield_now();
        }
        // Here I would do kernel things, IF I HAD ANY
        match u2k.read() {
            Some(msg) => {
                // println!("{:?}", &msg);
                // msg.release();
                sleep(Duration::from_millis(500));
                unimplemented!("{:?}", msg.deref());
            }
            None => {
                KERNEL_LOCK.store(false, Ordering::Release);
            }
        }
    }
}

fn userspace_entry() {
    let u2k = unsafe { BBBuffer::take_framed_producer(abi::U2K_RING.load(Ordering::Acquire)) };
    let _k2u = unsafe { BBBuffer::take_framed_consumer(abi::K2U_RING.load(Ordering::Acquire)) };

    loop {
        while KERNEL_LOCK.load(Ordering::Acquire) {
            yield_now();
        }

        // ...
        match u2k.grant(4) {
            Ok(mut gr) => {
                println!("Sending...");
                gr.copy_from_slice(&[1, 2, 3, 4]);
                gr.commit(4);
                println!("Sent!");

                sleep(Duration::from_millis(100));
                KERNEL_LOCK.store(true, Ordering::Release);
                sleep(Duration::from_millis(100));

                unimplemented!()
            },
            Err(_) => {
                println!("WHAT");
                panic!();
            },
        }
    }
}