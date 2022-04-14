//! Handle processes
//!
//! Create processes, and determine which one to run next
//!

use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::structures::paging::PageTableFlags;

use spin::RwLock;
use lazy_static::lazy_static;
extern crate alloc;
use alloc::{boxed::Box, collections::vec_deque::VecDeque, vec::Vec};

use core::arch::asm;

use crate::println;
use crate::interrupts::{Context, INTERRUPT_CONTEXT_SIZE};

use crate::gdt;
use crate::memory;

use object::{Object, ObjectSegment};

/// Size of the kernel stack for each process, in bytes
const KERNEL_STACK_SIZE: usize = 4096 * 2;

/// Size of the user stack for each user process, in bytes
const USER_STACK_SIZE: usize = 4096 * 5;

lazy_static! {
    /// Queue of processes which can run
    ///
    /// Notes:
    ///  - Threads are added to the back of the queue with push_back
    ///  - The next thread to run is removed from the front with pop_front
    static ref RUNNING_QUEUE: RwLock<VecDeque<Box<Thread>>> =
        RwLock::new(VecDeque::new());

    /// The process which is currently running
    static ref CURRENT_THREAD: RwLock<Option<Box<Thread>>> = RwLock::new(None);
}

/// Per-thread state
///
///
/// https://samwho.dev/blog/context-switching-on-x86/
///
/// Notes:
///  - Box::new(Thread { .. }) first constructs a new Thread
///    on the stack, then moves it onto the heap. Fixed sized arrays
///    therefore can't be used for the new process' stack because they
///    overflow the current stack.
struct Thread {
    /// Thread ID
    tid: usize,

    /// Page table physical address
    ///
    /// Note: Functions which manipulate page tables may temporarily
    /// modify their page table. To avoid having to disable
    /// interrupts, each thread's page table is saved and restored
    /// during context switches
    page_table_physaddr: u64,

    /// Kernel stack needed to handle system calls
    /// and interrupts including
    /// save/restore process state in context switch
    kernel_stack: Vec<u8>,

    /// Address of the end of the stack.
    /// This value is put in the Interrupt Stack Table
    kernel_stack_end: u64,

    /// Address within the kernel_stack which stores
    /// the Context structure containing thread state.
    context: u64,

    /// User stack. Note that kernel threads also
    /// use this stack
    user_stack: Vec<u8>
}

use core::fmt;

/// Enable Thread structs to be printed
impl fmt::Display for Thread {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Cast context address to Context struct
        let context = unsafe {&mut *(self.context as *mut Context)};

        let kernel_stack_start = VirtAddr::from_ptr(self.kernel_stack.as_ptr()).as_u64();
        let user_stack_start = VirtAddr::from_ptr(self.user_stack.as_ptr()).as_u64();

        write!(f, "\
TID: {}, rip: {:#016X}
    Kernel stack: {:#016X} - {:#016X} Context: {:#016X}
    Thread stack: {:#016X} - {:#016X} RSP: {:#016X}",
               self.tid, context.rip,
               // Second line
               kernel_stack_start,
               kernel_stack_start + (KERNEL_STACK_SIZE as u64),
               self.context,
               // Third line
               user_stack_start,
               user_stack_start + (USER_STACK_SIZE as u64),
               context.rsp)
    }
}

/// Start a new kernel thread, by adding it to the process table.
/// This won't run immediately, but will run when the scheduler
/// next switches to it.
///
/// Inputs
/// ------
///
/// function : fn() -> ()
///    The new thread entry point
///
/// Returns
/// -------
/// The TID of the new thread
///
pub fn new_kernel_thread(function: fn()->()) -> usize {
    // Create a new process table entry
    //
    // Note this is first created on the stack, then moved into a Box
    // on the heap.
    let new_thread = {
        let kernel_stack = Vec::with_capacity(KERNEL_STACK_SIZE);
        let kernel_stack_start = VirtAddr::from_ptr(kernel_stack.as_ptr());
        let kernel_stack_end = (kernel_stack_start + KERNEL_STACK_SIZE).as_u64();

        Box::new(Thread {
            tid: 0,
            page_table_physaddr: 0, // Don't need to switch PT
            kernel_stack,
            // Note that stacks move backwards, so SP points to the end
            kernel_stack_end,
            // Push a Context struct on the kernel stack
            context: kernel_stack_end - INTERRUPT_CONTEXT_SIZE as u64,
            user_stack: Vec::with_capacity(USER_STACK_SIZE)
        })
    };

    // Cast context address to Context struct
    let context = unsafe {&mut *(new_thread.context as *mut Context)};

    // Set the instruction pointer
    context.rip = function as usize;

    // Set flags
    unsafe {
        asm!{
            "pushf",
            "pop rax", // Get RFLAGS in RAX
            lateout("rax") context.rflags,
        }
    }

    context.cs = 8; // Code segment flags

    // The kernel thread has its own stack
    // Note: Need to point to the end of the memory region
    //       because the stack moves down in memory
    context.rsp = (VirtAddr::from_ptr(new_thread.user_stack.as_ptr()) + USER_STACK_SIZE).as_u64() as usize;

    let tid = new_thread.tid;

    println!("New kernel thread {}", new_thread);

    // Turn off interrupts while modifying process table
    interrupts::without_interrupts(|| {
        RUNNING_QUEUE.write().push_back(new_thread);
    });
    tid
}

pub fn new_user_thread(bin: &[u8]) -> Result<usize, &'static str> {
    // Check the header
    const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

    if bin[0..4] != ELF_MAGIC {
        return Err("Expected ELF binary");
    }
    // Use the object crate to parse the ELF file
    // https://crates.io/crates/object
    if let Ok(obj) = object::File::parse(bin) {

        // Create a user pagetable with only kernel pages
        let (user_page_table_ptr, user_page_table_physaddr) =
            memory::create_kernel_only_pagetable();

        // Store the page table and switch back before returning
        let original_page_table = memory::active_pagetable_physaddr();

        // Switch to the new user page table
        // Note: This only works because schedule_next() saves the
        //       page table for each thread. This thread temporarily has
        //       a different page table to the other threads
        memory::switch_to_pagetable(user_page_table_physaddr);

        let entry_point = obj.entry();
        println!("Entry point: {:#016X}", entry_point);

        for segment in obj.segments() {
            let segment_address = segment.address() as u64;

            println!("Section {:?} : {:#016X}", segment.name(), segment_address);

            if let Ok(data) = segment.data() {
                println!("  len : {}", data.len());

                // Allocate memory in the pagetable
                //
                // NOTE (FIXME): Need to check that memory range is not overlapping
                // kernel memory before allocating.
                memory::allocate_pages(user_page_table_ptr,
                                       VirtAddr::new(segment_address), // Start address
                                       data.len() as u64, // Size (bytes)
                                       PageTableFlags::PRESENT |
                                       PageTableFlags::WRITABLE |
                                       PageTableFlags::USER_ACCESSIBLE);

                // Copy data
                let dest_ptr = segment_address as *mut u8;
                for (i, value) in data.iter().enumerate() {
                    unsafe {
                        let ptr = dest_ptr.add(i);
                        core::ptr::write(ptr, *value);
                    }
                }
            } else {
                // Switch back
                memory::switch_to_pagetable(original_page_table);
                return Err("Could not get segment data");
            }
        }
        // At this point we can switch back to the original page table
        memory::switch_to_pagetable(original_page_table);

        // Create the new Thread struct
        let new_thread = {
            let kernel_stack = Vec::with_capacity(KERNEL_STACK_SIZE);
            let kernel_stack_start = VirtAddr::from_ptr(kernel_stack.as_ptr());
            let kernel_stack_end = (kernel_stack_start + KERNEL_STACK_SIZE).as_u64();

            Box::new(Thread {
                tid: 0,
                page_table_physaddr: user_page_table_physaddr,
                kernel_stack,
                // Note that stacks move backwards, so SP points to the end
                kernel_stack_end,
                // Push a Context struct on the kernel stack
                context: kernel_stack_end - INTERRUPT_CONTEXT_SIZE as u64,
                // User stack needs new pages, not allocated on the kernel heap
                user_stack: Vec::new()
            })
        };

        // Cast context address to Context struct
        let context = unsafe {&mut *(new_thread.context as *mut Context)};

        context.rip = entry_point as usize;

        // Set flags
        context.rflags = 0x0200; // Interrupt enable

        let (code_selector, data_selector) = gdt::get_user_segments();
        context.cs = code_selector.0 as usize; // Code segment flags
        context.ss = data_selector.0 as usize; // Without this we get a GPF

        // Allocate pages for the user stack
        const USER_STACK_START: u64 = 0x5200000;

        memory::allocate_pages(user_page_table_ptr,
                               VirtAddr::new(USER_STACK_START), // Start address
                               USER_STACK_SIZE as u64, // Size (bytes)
                               PageTableFlags::PRESENT |
                               PageTableFlags::WRITABLE |
                               PageTableFlags::USER_ACCESSIBLE);

        // Note: Need to point to the end of the allocated region
        //       because the stack moves down in memory
        context.rsp = (USER_STACK_START as usize) + USER_STACK_SIZE;

        let tid = new_thread.tid;

        println!("New Thread {}", new_thread);
        // No interrupts while modifying queue
        interrupts::without_interrupts(|| {
            RUNNING_QUEUE.write().push_back(new_thread);
        });

        return Ok(tid);
    }
    return Err("Could not parse ELF");
}

/// This is called by the timer interrupt handler
///
/// Returns the stack containing the process state
/// (interrupts::Context struct)
pub fn schedule_next(context: &Context) -> usize {

    let mut running_queue = RUNNING_QUEUE.write();
    let mut current_thread = CURRENT_THREAD.write();

    if let Some(thread) = current_thread.take() {
        // Put the current thread to the back of the queue

        // Update the stack pointer
        let mut thread_mut = thread;

        // Store context location. This should almost always be in the same
        // location on the kernel stack. The exception is the
        // first time a context switch occurs from the original kernel
        // stack to the first kernel thread stack.
        thread_mut.context = (context as *const Context) as u64;

        // Save the page table. This is to enable context
        // switching during functions which manipulate page tables
        // for example new_user_thread
        thread_mut.page_table_physaddr = memory::active_pagetable_physaddr();

        running_queue.push_back(thread_mut);
    }
    *current_thread = running_queue.pop_front();

    match current_thread.as_ref() {
        Some(thread) => {
            // Set the kernel stack for the next interrupt
            gdt::set_interrupt_stack_table(
                gdt::TIMER_INTERRUPT_INDEX as usize,
                // Note: Point to the end of the stack
                VirtAddr::new(thread.kernel_stack_end));

            if thread.page_table_physaddr != 0 {
                // Change page table
                // Note: zero for kernel thread
                memory::switch_to_pagetable(thread.page_table_physaddr);
            }

            // Point the stack to the new context
            // (which is usually stored on the kernel stack)
            thread.context as usize
        },
        None => 0
    }
}
