extern crate hash_hasher;
extern crate num_cpus;
extern crate ripline;

use crossbeam_channel::unbounded;
// Special hasher for already hashed data - NTLM is a hash
use clap::Parser;
use memmap2::Mmap;
use std::error::Error;
use std::fs::File;
use std::io::{stdout, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::thread;
use std::thread::JoinHandle;
use std::time::Instant;
use regex::bytes::Regex;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // Command line arguments /*{{{*/

    /// String of what to search for
    tofind: String,

    /// File in which to search for matches
    wordlist: PathBuf,

    /// Match a full-line exactly
    #[arg(short, long)]
    exact: bool,

    /// Find only the first match
    #[arg(short, long)]
    first: bool,

    /// Search for a regex string, not fixed bytes
    #[arg(short, long)]
    regex: bool,

    /// Display verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Print the byte position at which the match was made
    #[arg(short, long)]
    position: bool,

    /// File read block size
    #[arg(short, long, default_value_t = 8_388_608)]
    block: usize,

    /// Cache block size
    #[arg(short, long, default_value_t = 2_147_483_648)]
    cache: usize,

    /// Thread shard size
    #[arg(short, long, default_value_t = 393_728)]
    shard: usize,
}
/*}}}*/

// BSD/macOS and Linux use different uncache calls msync vs fadvise
#[cfg(target_os = "macos")]
use libc::{mincore, msync, MS_INVALIDATE};
#[cfg(target_os = "linux")]
use libc::{mincore, posix_fadvise, POSIX_FADV_DONTNEED};
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;

#[cfg(target_os = "macos")]
fn uncache(file: &Mmap, len: usize) {
    // Flush a part of the file from disk cache MacOS version/*{{{*/
    let ret = unsafe { msync(file.as_ptr() as _, len, MS_INVALIDATE) };
    assert!(ret == 0, "msync failed with error {}", ret);
}
/*}}}*/

#[cfg(target_os = "linux")]
fn uncache(file: &File, mmap: &mut Mmap, len: usize) {
    // Flush a part of the file from disk cache Linux version/*{{{*/
    let ret = unsafe { posix_fadvise(file.as_raw_fd() as _, 0, len as i64, POSIX_FADV_DONTNEED) };
    assert!(ret == 0, "posix_fadvise failed with error {}", ret);

    // The need for this re-mmap below is confusing, here's what I know so far: A
    // vanilla PoC that opens a file and mmap reads from the mmap and does the
    // cache'ing and drop'ing like we do here, works fine on linux. But when
    // applied like we do here, the drop'ing doesn't work. Even if I comment out
    // the reading from the mmap. I've no idea why. But if I redo the mmap, it
    // will respect the drop. When I get round to debugging I'll start here
    // https://github.com/torvalds/linux/blob/786b71f5b754273ccef6d9462e52062b3e1f9877/mm/fadvise.c#L119
    *mmap = unsafe { Mmap::map(file).unwrap() };
}
/*}}}*/

fn mincore_check(file: &Mmap, len: usize, answer: &mut [u8]) {
    // Check what part of the file is in disk cache /*{{{*/
    #[cfg(target_os = "linux")]
    let ret = unsafe { mincore(file.as_ptr() as _, len, answer.as_mut_ptr().cast::<u8>()) };
    #[cfg(target_os = "macos")]
    let ret = unsafe { mincore(file.as_ptr() as _, len, answer.as_mut_ptr().cast::<i8>()) };

    assert!(ret == 0, "mincore failed with error {}", ret);
}
/*}}}*/

fn gen_stats(answer: &[u8], pages: usize) -> f64 {
    // Calculate and print disk cache stats /*{{{*/
    let in_cache: usize = answer.iter().map(|x| (x & 0x1) as usize).sum();
    assert!(in_cache <= f64::MAX as usize); // ensure safe usize -> f64 conversion
    assert!(pages <= f64::MAX as usize); // ensure safe usize -> f64 conversion
                                         //let percent_cached = (in_cache as f64 / pages as f64) * 100_f64;
                                         //println!("[+] Pages in cache {in_cache}/{pages} ({percent_cached:.2}%)");
    (in_cache as f64 / pages as f64) * 100_f64
}
/*}}}*/

fn cache_file(file: &mut File, length: usize, block_size: usize, offset: u64) -> f64 {
    // Cache part of the file to disk cache using read() on the file (not mmap) /*{{{*/
    let mut junk = vec![0u8; block_size];
    file.seek(SeekFrom::Start(offset)).unwrap();
    //let start = Instant::now();
    for _ in 0..=(length / block_size) {
        // the read is not handled because we're only doing it to encourage the
        // kernel to cache the file. Ignore clippy's error.
        file.read(&mut junk).unwrap();
    }
    0.0 // return this if we aren't timing, otherwise uncomment below
        /*
          let elapsed = (start.elapsed().as_secs() as f64)
                        + (f64::from(start.elapsed().subsec_nanos()) / 1_000_000_000.0);
          println!("[+] Read {length} bytes in {elapsed:.2} s ({:.2} GB/s)",
            (length as f64 / elapsed) / 1024.0 / 1024.0 / 1024.0);
          elapsed
        */
}
/*}}}*/

#[derive(Clone)]
struct ToFind {
    // Structuroe to hold our value to find /*{{{*/
    value: Vec<u8>,
    start: [bool; 256],
    second: [bool; 256],
}
/*}}}*/

fn parse_tofind(tofind: &str) -> Result<ToFind, Box<dyn Error>> {
    // Turn input hashes into required data structures /*{{{*/
    let value: Vec<u8> = tofind.try_into()?;

    // store the first and last byte of input hashes, so for small input hash lists
    // we can do a cheaper check than a hashmap lookup
    let mut start = [false; 256];
    let mut second = [false; 256];

    start[value[0] as usize] = true;
    if value.len() > 1 {
        second[value[1] as usize] = true;
    }

    Ok(ToFind {
        value,
        start,
        second,
    })
}
/*}}}*/

struct Wordlist {
    // Structure to hold our wordlist stats /*{{{*/
    file: File,
    mmap: Mmap,
    cache_point: usize,
    length: usize,
    pages: usize,
    cache_size: usize,
}
/*}}}*/

fn initialise_wordlist(
    path: &PathBuf,
    cache_size: usize,
    block_size: usize,
    verbose: bool,
) -> Result<Wordlist, Box<dyn Error>> {
    // Read and cache the start of the wordlist /*{{{*/
    let mut wordlist_file = File::open(&path)?;
    let wordlist_mmap = unsafe { Mmap::map(&wordlist_file)? };

    let page_size = page_size::get();
    let wordlist_length = wordlist_mmap.len();
    let wordlist_pages = (wordlist_length + page_size - 1) / page_size;
    let cache_point;

    let mut answer = vec![0u8; wordlist_pages];
    mincore_check(&wordlist_mmap, wordlist_length, &mut answer);
    let mut percent_cached: f64 = gen_stats(&answer, wordlist_pages);
    if verbose {
        println!("[+] Wordlist is {wordlist_length} bytes and {wordlist_pages} pages, currently {percent_cached:.2}% cached");
    }

    if percent_cached < 97.0 {
        if wordlist_length > cache_size {
            let _elapsed_time = cache_file(&mut wordlist_file, cache_size, block_size, 0);
            mincore_check(&wordlist_mmap, wordlist_length, &mut answer);
            percent_cached = gen_stats(&answer, wordlist_pages);
            assert!(wordlist_length <= f64::MAX as usize); // safe f64 conversion
            if verbose && percent_cached >= (wordlist_length / cache_size) as f64 {
                println!("[*] Successfully cached first part of wordlist");
            }
            cache_point = cache_size;
        } else {
            let _elapsed_time = cache_file(&mut wordlist_file, wordlist_length, block_size, 0);
            mincore_check(&wordlist_mmap, wordlist_length, &mut answer);
            percent_cached = gen_stats(&answer, wordlist_pages);
            if verbose && percent_cached >= 95.0 {
                println!("Successfully cached wordlist");
            }
            cache_point = wordlist_length;
        }
    } else {
        if verbose {
            println!("Wordlist already cached");
        }
        cache_point = wordlist_length;
    }

    Ok(Wordlist {
        file: wordlist_file,
        mmap: wordlist_mmap,
        cache_point,
        length: wordlist_length,
        pages: wordlist_pages,
        cache_size,
    })
}
/*}}}*/

struct Workers {
    // Structure to hold our thread worker info /*{{{*/
    threadnum: usize,
    threadhand: Vec<JoinHandle<()>>,
    tx: crossbeam_channel::Sender<Option<(usize, Vec<u8>)>>,
    //rx: crossbeam_channel::Receiver<Option<Vec<u8>>>,
    //tx2: crossbeam_channel::Sender<Stats>,
    rx2: crossbeam_channel::Receiver<Stats>,
}
/*}}}*/

#[derive(Clone, Copy)] // needed to send via channels between thread and main
struct Stats {
    // Structure to hold counters from the threads /*{{{*/
    cracked: usize,
    hashed: usize,
    waits: usize,
    kbs: usize,
}
/*}}}*/

fn find(tofind: &ToFind, clear: &[u8]) -> bool {
    // Matching routine /*{{{*/

    // for small hashlists, can we get away with this cheaper check
    if !tofind.start[clear[0] as usize]
    {
        return false;
    }
    if clear.len() > 1 && !tofind.second[clear[1] as usize]
    {
        return false;
    }

    // check if the generated hash is in our input hash list
    if tofind.value == clear {
        return true;
    }

    false
}
/*}}}*/

fn setup_workers(tofind: &ToFind, args: &Args) -> Workers {
    // Fire off our worker threads to wait for the data from the wordlist /*{{{*/
    let threadnum = num_cpus::get(); // set the number of threads to the number of cores
    let mut threadhand: Vec<JoinHandle<_>> = Vec::new();
    let updatethresh = 1; // how often to update the main thread

    let re = Regex::new(&args.tofind).unwrap();

    // We clone the reciever multiple times which is how the threads pick up new clears
    // Can't do that with mpsc which only allows cloning the sender, need crossbeam
    let (tx, rx): (
        crossbeam_channel::Sender<Option<(usize, Vec<u8>)>>,
        crossbeam_channel::Receiver<Option<(usize, Vec<u8>)>>,
    ) = unbounded();
    let (tx2, rx2): (
        crossbeam_channel::Sender<Stats>,
        crossbeam_channel::Receiver<Stats>,
    ) = unbounded();

    for _ in 0..threadnum {
        //for j in 0..threadnum {
        // Make copies of these two for the threads
        let rx_thread = rx.clone();
        let tx2_thread = tx2.clone();
        let tofind_thread = tofind.clone();
        let args_exact = args.exact;
        let args_position = args.position;
        let args_regex = args.regex;
        let re_thread = re.clone();
        //let to_find_thread = hashes.hashlist.clone();
        threadhand.push(thread::spawn(move || {
            // The in-thread worker code /*{{{*/
            // Pre-allocate to reduce alloc overhead
            let mut out: Vec<u8> = Vec::with_capacity(8192);
            let mut stats = Stats {
                cracked: 0,
                hashed: 0,
                waits: 0,
                kbs: 0, // not used here
            };

            // Fetch clears from the channel
            loop {
                //for recv in rx_thread {
                if let Ok(recv) = rx_thread.try_recv() {
                    // We wrap the message in an Option to allow for a kill signal
                    // Our thread recieved None lets dump our buffer and exit
                    if recv == None {
                        //println!("Break {}",j);
                        stdout().write_all(&out).unwrap();
                        tx2_thread.send(stats).unwrap();
                        break;
                    }
                    // We got some clears to crack
                    if let Some((mut pos, message)) = recv {
                        for clear in message.split(|c| *c == 10_u8).filter(|l| !l.is_empty()) {
                            stats.hashed += 1;
                            pos += clear.len() + 1;
                            let mut found = false;
                            //println!("Thread {} recieved: '{:?}'",j,std::str::from_utf8(clear));

                            // encoding error
                            if clear.is_empty() {
                                continue;
                            }

                            if args_regex {
                                found = re_thread.is_match(clear);
                            } else { //regexp
                                if args_exact {
                                    found = find(&tofind_thread, clear);
                                    if !found { continue; }
                                } else {
                                    // Not exact match
                                    for sub in clear.windows(tofind_thread.value.len()) {
                                        found = find(&tofind_thread, sub);
                                        if found { break; }
                                    }
                                }
                            }

                            if found {
                                stats.cracked += 1;
                                if args_position {
                                    write!(&mut out, "{}", pos-clear.len()-1).unwrap();
                                    // extend_from_slice is faster than push
                                    out.extend_from_slice(&[58]); // colon
                                }
                                out.extend_from_slice(clear); // clear text
                                out.extend_from_slice(&[10]); // newline
                                                              // check if our output buffer should be flushed
                                if out.len() >= 8192 {
                                    // make sure this comparison aligns with capacity
                                    stdout().write_all(&out).unwrap();
                                    out.clear();
                                }
                                // update the main process on progress
                                if stats.cracked == updatethresh {
                                    tx2_thread.send(stats).unwrap();
                                    stats.cracked = 0;
                                    stats.hashed = 0;
                                }
                            }
                        }
                    }
                }
                while rx_thread.is_empty() {
                    stats.waits += 1;
                    //write!(&stdout(),"{}.",count).unwrap();
                    thread::sleep(std::time::Duration::from_millis(stats.waits as u64));
                }
            }
        }));
        /*}}}*/
    }
    Workers {
        threadnum,
        threadhand,
        tx,
        //rx: rx,
        //tx2: tx2,
        rx2,
    }
}
/*}}}*/

fn read_wordlist(
    wordlist: &mut Wordlist,
    chunk_size: usize,
    workers: &Workers,
    first: bool,
    block_size: usize,
) -> Result<Stats, Box<dyn Error>> {
    // Read the wordlist, send chunks to the worker threads & handle cache'ing /*{{{*/
    let mut stats = Stats {
        cracked: 0, // how many have we cracked
        hashed: 0,  // how many hashes have we generated
        waits: 0,   // how many times was a thread waiting
        kbs: 0,     // amount of data read for perf stats
    };
    let mut count = 1; // optimisation counter to reduce expensive thread checkins
    let check_thresh = 50; // how often to check with the threads

    // Send chunks of the wordlist to the threads to deal with, but split on newlines
    let mut pos = 0; // our current pointer/index into the wordlist
    while pos < wordlist.length - 1 {
        // advance the cursor but not past the end of the file
        let mut to = match pos {
            e if e + chunk_size >= wordlist.length => wordlist.length,
            _ => pos + chunk_size,
        };
        // find a newline to end on to save threads having to do it
        while wordlist.mmap[to - 1] != 10 && to < wordlist.length {
            to += 1;
        }
        // send it to the threads
        workers
            .tx
            .send(Some((pos, wordlist.mmap[pos..to].to_vec())))?;
        // update the bytes counter
        stats.kbs += (to - pos) / 1024;
        // update the cursor position
        pos = to - 1;
        // only checkin with threads sometimes to prevent slowdowns
        if count % check_thresh == 0 {
            // check if we can exit early because we cracked everything
            if let Ok(recv_stats) = workers.rx2.try_recv() {
                stats.cracked += recv_stats.cracked;
                stats.hashed += recv_stats.hashed;
                stats.waits += recv_stats.waits;
                // if we can exit early stop reading the wordlist and try exit
                if stats.cracked == 1 && first {
                    break;
                }
            }
        }
        count += 1;

        // Once we've read half the cache'd data, drop the first half, and cache ahead another half
        if pos % (wordlist.cache_size / 2) <= chunk_size && wordlist.cache_point < wordlist.length {
            // Drop the first half of the cache'd data
            #[cfg(target_os = "macos")]
            uncache(&wordlist.mmap, pos);
            #[cfg(target_os = "linux")]
            uncache(&wordlist.file, &mut wordlist.mmap, pos);

            // Cache the next half block
            let _elapsed_time = cache_file(
                &mut wordlist.file,
                wordlist.cache_size / 2,
                block_size,
                wordlist.cache_point as u64,
            );
            wordlist.cache_point = match wordlist.cache_size {
                _ if (wordlist.cache_point + wordlist.cache_size / 2) >= wordlist.length => {
                    wordlist.length
                }
                _ => (wordlist.cache_point + wordlist.cache_size / 2),
            };
            /*
              // Some debugging stats
              let mut percent_cached: f64 = 0.0;
              let mut answer = vec![0u8; wordlist.pages];
              mincore_check(&wordlist.mmap, wordlist.length, &mut answer);
              percent_cached = gen_stats(&answer, wordlist.pages);
              println!("[+] Purging up first {:.2}% bytes from cache
            Cache point now at {:.2}%, Total in cache now {percent_cached:.2}%",(pos as f64/wordlist.length as f64) * 100_f64,(wordlist.cache_point as f64/wordlist.length as f64) *100_f64);
              */
        }
    }
    Ok(stats)
}
/*}}}*/

fn main() -> Result<(), Box<dyn Error>> {
    // Put it all together /*{{{*/
    let args = Args::parse();

    // Put the input hashes (to be cracked) into the required forms
    let tofind = parse_tofind(&args.tofind)?;

    // Do some dd tests to find optimal block size for your HD
    // Here's an example, 1M is repeated to warm the file into cache
    // e.g. for x in 1M 1M 2M 4M 8M 12M; do time dd if=somefile of=/dev/null bs=$x; done
    //let block_size = 1_048_576; //1M
    //let block_size = 8_388_608; //8M
    let block_size = args.block;

    // How big are the cache chunks you want to use
    // It depends on how big the file cache on your system can grow. On my tested
    // systems, it's about 68% of total system memory (mac `sysctl hw.memsize`
    // linux `cat /proc/meminfo |head -n1`). But if there's a ton of stuff
    // running it will be partially filled and you'll have less space. MS Teams
    // is a great example of this.
    //let cache_size = 10_737_418_240; //10G
    //let cache_size = 5_368_709_120; //5G
    //let cache_size = 4_294_967_296; //4G
    //let cache_size = 2_147_483_648; //2G
    //let cache_size = 1_073_741_824; //1G
    //let cache_size = 536_870_912; //512M
    //let cache_size = 268_435_456; //256M
    let cache_size = args.cache;

    // size of wordlist chunk to send to thread
    // if you're seeing too many waits, try optimising this by taking it via cmd
    // line arg below and testing different sizes. 393k works well on a M1 Pro
    // MBP.
    //let chunk_size = 393_728;
    //let chunk_size = 5_248_000;
    let chunk_size = args.shard;

    // Build the wordlist (the clears to hash and check for a match)
    let mut wordlist = initialise_wordlist(&args.wordlist, cache_size, block_size, args.verbose)?;
    let workers = setup_workers(&tofind, &args);
    let start = Instant::now();
    let mut stats = read_wordlist(&mut wordlist, chunk_size, &workers, args.first, block_size)?;
    // All done reading the wordlist, now it's up to the threads to finish

    // Make sure the workers have picked up all the chunks
    loop {
        if workers.tx.is_empty() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(2_u64));
    }
    // tell the threads to exit, as many times as there are threads
    for _ in 0..workers.threadnum {
        workers.tx.send(None)?;
    }
    // wait for threads to exit
    // Don't try put this in a function JoinHandle<()> doesn't implement Copy
    for thread in workers.threadhand {
        thread.join().unwrap();
    }

    if args.verbose {
        // get final numbers
        while let Ok(recv_stats) = workers.rx2.try_recv() {
            stats.cracked += recv_stats.cracked;
            stats.hashed += recv_stats.hashed;
            stats.waits += recv_stats.waits;
        }

        // calculate performance stats
        let elapsed = (start.elapsed().as_secs() as f64)
            + (f64::from(start.elapsed().subsec_nanos()) / 1_000_000_000.0);
        //safe usize->f64 conversion checks
        assert!(stats.hashed <= f64::MAX as usize);
        assert!(stats.kbs <= f64::MAX as usize);
        assert!(stats.waits <= f64::MAX as usize);
        print!(
            "[+] Stats:
      Time: {:.2} s
      Lines Checked: {}, Found: {}, Find Speed: {:.2} kF/s
      Read: {} kB, Read Speed: {:.2} MB/s
      Thread Waits: {} Wait Speed: {:.2} w/s\n",
            elapsed,
            stats.hashed,
            stats.cracked,
            (stats.hashed as f64 / elapsed) / 1024_f64,
            stats.kbs,
            (stats.kbs as f64 / elapsed) / 1024_f64,
            stats.waits,
            stats.waits as f64 / elapsed
        );
    }

    Ok(())
}
/*}}}*/
