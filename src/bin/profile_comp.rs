/// Duplicate code here for analysis with VTune
extern crate lz4_flex;


const COMPRESSION10MB: &'static [u8] = include_bytes!("../../benches/dickens.txt");

fn main() {
    // use cpuprofiler::PROFILER;
    // PROFILER.lock().unwrap().start("./my-prof.profile").unwrap();
    for _ in 0..100 {
        compress(COMPRESSION10MB as &[u8]);
    }
    // compress(COMPRESSION10MB as &[u8]);
    // PROFILER.lock().unwrap().stop().unwrap();
}

const LZ4_SKIPTRIGGER: usize = 6;
const MFLIMIT: u32 = 12;
static LZ4_MIN_LENGTH: u32 = MFLIMIT+1;
const MINMATCH: usize = 4;
#[allow(dead_code)]
const LZ4_HASHLOG: u32 = 12;

const MAXD_LOG: usize = 16;
const MAX_DISTANCE: usize = (1 << MAXD_LOG) - 1;
const END_OFFSET: usize = 5;
/// Switch for the hashtable size byU16
// #[allow(dead_code)]
// static LZ4_64KLIMIT: u32 = (64 * 1024) + (MFLIMIT - 1);


pub(crate) fn hash(sequence:u32) -> u32 {
    let res = (sequence.wrapping_mul(2654435761_u32))
            >> (1 + (MINMATCH as u32 * 8) - (LZ4_HASHLOG + 1));
    res
}

fn wild_copy_from_src(mut source: *const u8, mut dst_ptr: *mut u8, num_items: usize) {
    // output.reserve(num_items);
    unsafe{

        // let mut dst_ptr = output.as_mut_ptr().add(output.len());
        let dst_ptr_end = dst_ptr.add(num_items);

        while dst_ptr < dst_ptr_end {
            std::ptr::copy_nonoverlapping(source, dst_ptr, 8);
            source = source.add(8);
            dst_ptr = dst_ptr.add(8);
        }
    }
}


/// An LZ4 encoder.
struct Encoder{
    /// The raw uncompressed input.
    input: *const u8,
    input_size: usize,
    /// The compressed output.
    output_ptr: *mut u8,
    /// The number of bytes from the input that are encoded.
    cur: usize,
    /// Shift the hash value for the dictionary to the right, so match the dictionary size.
    dict_bitshift: u8,
    /// The dictionary of previously encoded sequences.
    ///
    /// This is used to find duplicates in the stream so they are not written multiple times.
    ///
    /// Every four bytes are hashed, and in the resulting slot their position in the input buffer
    /// is placed. This way we can easily look up a candidate to back references.
    dict: Vec<usize>,
}

impl Encoder {
    /// Get the hash of the current four bytes below the cursor.
    ///
    /// This is guaranteed to be below `DICTIONARY_SIZE`.
    #[inline]
    fn get_cur_hash(&self) -> usize {
        hash(self.get_batch(self.cur)) as usize >> self.dict_bitshift
    }

    #[inline]
    fn get_hash_at(&self, pos: usize) -> usize {
        hash(self.get_batch(pos)) as usize >> self.dict_bitshift
    }

    /// Read a 4-byte "batch" from some position.
    ///
    /// This will read a native-endian 4-byte integer from some position.
    #[inline]
    fn get_batch(&self, n: usize) -> u32 {
        let mut batch:u32 = 0;
        unsafe{std::ptr::copy_nonoverlapping(self.input.add(n), &mut batch as *mut u32 as *mut u8, 4);} 
        batch
    }

    /// Write an integer to the output in LSIC format.
    #[inline]
    fn write_integer(&mut self, mut n: usize) -> std::io::Result<()> {
        // Write the 0xFF bytes as long as the integer is higher than said value.
        while n >= 0xFF {
            n -= 0xFF;
            unsafe{
                self.output_ptr.write(0xFF);
                self.output_ptr = self.output_ptr.add(1);
            }
        }

        // Write the remaining byte.
        unsafe{
            self.output_ptr.write(n as u8);
            self.output_ptr = self.output_ptr.add(1);
        }
        Ok(())
    }

    /// Read the batch at the cursor.
    #[inline(never)]
    unsafe fn count_same_bytes(&self, first: *const u8, second: *const u8) -> usize {
        let mut pos = 0;

        // compare 4/8 bytes blocks depending on the arch
        const STEP_SIZE: usize = std::mem::size_of::<usize>();
        while self.cur + pos + STEP_SIZE + END_OFFSET < self.input_size  {
            let diff = read_usize_ptr(first.add(pos)) ^ read_usize_ptr(second.add(pos));

            if diff == 0{
                pos += STEP_SIZE;
                continue;
            }else {
                return pos + get_common_bytes(diff) as usize;
            }
        }

        // compare 4 bytes block
        #[cfg(target_pointer_width = "64")]{
            if self.cur + pos + 4 + END_OFFSET < self.input_size  {
                let diff = read_u32_ptr(first.add(pos)) ^ read_u32_ptr(second.add(pos));

                if diff == 0{
                    return pos + 4;
                }else {
                    return pos + (diff.trailing_zeros() >> 3) as usize
                }
            }
        }
        
        // compare 2 bytes block
        if self.cur + pos + 2 + END_OFFSET < self.input_size  {
            let diff = read_u16_ptr(first.add(pos)) ^ read_u16_ptr(second.add(pos));

            if diff == 0{
                return pos + 2;
            }else {
                return pos + (diff.trailing_zeros() >> 3) as usize
            }
        }

        // TODO add end_pos_check, last 5 bytes should be literals
        if self.cur + pos + 1 + END_OFFSET < self.input_size  && first.read() == second.read(){
            pos +=1;
        }

        pos
    }

    /// Complete the encoding into `self.output`.
    #[inline]
    fn handle_last_literals(&mut self, start:usize, out_ptr_start: *mut u8) -> std::io::Result<usize> {

        let lit_len = self.input_size - start;
        // copy the last literals
        let token = if lit_len < 0xF {
            // Since we can fit the literals length into it, there is no need for saturation.
            (lit_len as u8) << 4
        } else {
            // We were unable to fit the literals into it, so we saturate to 0xF. We will later
            // write the extensional value through LSIC encoding.
            0xF0
        };
        push_unsafe(&mut self.output_ptr, token);
        if lit_len >= 0xF {
            self.write_integer(lit_len - 0xF)?;
        }

        // Now, write the actual literals.
        unsafe{
            wild_copy_from_src(self.input.add(start), self.output_ptr, lit_len); // TODO add wildcopy check 8byte
            self.output_ptr = self.output_ptr.add(lit_len);
        }
        return Ok(self.output_ptr as usize - out_ptr_start as usize);

    }

    #[inline(never)]
    fn complete(&mut self) -> std::io::Result<usize> {
        let out_ptr_start = self.output_ptr;
        /* Input too small, no compression (all literals) */
        if self.input_size < LZ4_MIN_LENGTH as usize {
            // The length (in bytes) of the literals section.
            let lit_len = self.input_size;
            let token = if lit_len < 0xF {
                // Since we can fit the literals length into it, there is no need for saturation.
                (lit_len as u8) << 4
            } else {
                // We were unable to fit the literals into it, so we saturate to 0xF. We will later
                // write the extensional value through LSIC encoding.
                0xF0
            };
            push_unsafe(&mut self.output_ptr, token);
            // self.output.push(token);
            if lit_len >= 0xF {
                self.write_integer(lit_len - 0xF)?;
            }

            // Now, write the actual literals.
            // self.output.extend_from_slice(&self.input);
            copy_into_vec(&mut self.output_ptr, self.input, self.input_size);
            return Ok(self.output_ptr as usize - out_ptr_start as usize);
        }
        let mut start = self.cur;
        let hash = self.get_cur_hash();
        unsafe{*self.dict.get_unchecked_mut(hash) = self.cur};
        self.cur += 1;
        let mut forward_hash = self.get_cur_hash();

        let end_pos_check = self.input_size - MFLIMIT as usize;
        loop {

            // Read the next block into two sections, the literals and the duplicates.
            let mut step_size;
            let mut non_match_count = 1 << LZ4_SKIPTRIGGER;
            // The number of bytes before our cursor, where the duplicate starts.
            // let mut offset: u16 = 0;

            let mut next_cur = self.cur;
            let mut candidate;

            while {
                
                non_match_count += 1;
                step_size = non_match_count >> LZ4_SKIPTRIGGER;

                let hash = forward_hash;
                self.cur = next_cur;
                next_cur += step_size;

                if self.cur > end_pos_check {
                    return self.handle_last_literals(start, out_ptr_start);
                }
                // Find a candidate in the dictionary with the hash of the current four bytes.
                // Unchecked is safe as long as the values from the hash function don't exceed the size of the table.
                // This is ensured by right shifting the hash values (`dict_bitshift`) to fit them in the table
                candidate = unsafe{*self.dict.get_unchecked(hash)};
                unsafe{*self.dict.get_unchecked_mut(hash) = self.cur};
                forward_hash = self.get_hash_at(next_cur);

                // Two requirements to the candidate exists:
                // - We should not return a position which is merely a hash collision, so w that the
                //   candidate actually matches what we search for.
                // - We can address up to 16-bit offset, hence we are only able to address the candidate if
                //   its offset is less than or equals to 0xFFFF.
                (candidate + MAX_DISTANCE) < self.cur ||
                self.get_batch(candidate) != self.get_batch(self.cur)
            }{}

            let offset = (self.cur - candidate) as u16;
            let match_length = unsafe{ self.count_same_bytes(self.input.add(self.cur+MINMATCH), self.input.add(candidate + MINMATCH)) };
            // The length (in bytes) of the literals section.
            let lit_len = self.cur - start;

            // Generate the higher half of the token.
            let mut token = if lit_len < 0xF {
                // Since we can fit the literals length into it, there is no need for saturation.
                (lit_len as u8) << 4
            } else {
                // We were unable to fit the literals into it, so we saturate to 0xF. We will later
                // write the extensional value through LSIC encoding.
                0xF0
            };

            // Generate the lower half of the token, the duplicates length.
            self.cur += match_length + 4;
            // self.go_forward_2(match_length + 4);
            token |= if match_length < 0xF {
                // We could fit it in.
                match_length as u8
            } else {
                // We were unable to fit it in, so we default to 0xF, which will later be extended
                // by LSIC encoding.
                0xF
            };

            // Push the token to the output stream.
            push_unsafe(&mut self.output_ptr, token);
            // If we were unable to fit the literals length into the token, write the extensional
            // part through LSIC.
            if lit_len >= 0xF {
                self.write_integer(lit_len - 0xF)?;
            }

            // Now, write the actual literals.
            unsafe{
                wild_copy_from_src(self.input.add(start), self.output_ptr, lit_len); // TODO add wildcopy check 8byte
                self.output_ptr = self.output_ptr.add(lit_len);
            }

            // write the offset in little endian.
            unsafe{
                std::ptr::copy_nonoverlapping(&offset.to_le() as *const u16 as *const u8, self.output_ptr, 2);
                self.output_ptr = self.output_ptr.add(2);
            } 
            // If we were unable to fit the duplicates length into the token, write the
            // extensional part through LSIC.
            if match_length >= 0xF {
                self.write_integer(match_length - 0xF)?;
            }
            start = self.cur;
            forward_hash = self.get_hash_at(next_cur);
        }
        // Ok(self.output_ptr as usize - out_ptr_start as usize)
    }
}

/// Compress all bytes of `input` into `output`.
#[inline(never)]
pub fn compress_into(input: &[u8], output: &mut Vec<u8>) -> std::io::Result<usize> {
    // TODO check dictionary sizes for input input_sizes
    let (dict_size, dict_bitshift) = match input.len() {
        0..=500 => (128, 9),
        500..=1_000 => (256, 8),
        1_000..=4_000 => (512, 7),
        4_000..=8_000 => (1024, 6),
        8_000..=16_000 => (2048, 5),
        16_000..=100_000 => (4096, 4),
        100_000..=400_000 => (8192, 3),
        _ => (16384, 2),
    };
    let dict = vec![0; dict_size];

    Encoder {
        input: input.as_ptr(),
        input_size: input.len(),
        output_ptr: output.as_mut_ptr(),
        dict_bitshift: dict_bitshift,
        cur: 0,
        dict,
    }.complete()
}

#[inline]
fn push_unsafe(output: &mut *mut u8, el: u8) {
    unsafe{
        std::ptr::write(*output, el);
        *output = output.add(1);
    }
}

/// Compress all bytes of `input`.
#[inline(never)]
pub fn compress(input: &[u8]) -> Vec<u8> {
    // In most cases, the compression won't expand the size, so we set the input size as capacity.
    let mut vec = Vec::with_capacity(16 + (input.len() as f64 * 1.1) as usize);

    let bytes_written = compress_into(input, &mut vec).unwrap();
    unsafe{
        vec.set_len(bytes_written);
    }
    vec
}

#[inline]
fn copy_into_vec(out_ptr:&mut *mut u8, start: *const u8, num_items: usize) {
    // vec.reserve(num_items);
    unsafe {
        std::ptr::copy_nonoverlapping(start, *out_ptr, num_items);
        *out_ptr = out_ptr.add(num_items);
        // vec.set_len(vec.len() + num_items);
    }
}
// fn copy_into_vec(vec: &mut Vec<u8>, start: *const u8, num_items: usize) {
//     // vec.reserve(num_items);
//     unsafe {
//         std::ptr::copy_nonoverlapping(start, vec.as_mut_ptr().add(vec.len()), num_items);
//         vec.set_len(vec.len() + num_items);
//     }
// }


// fn read_u64_ptr(input: *const u8) -> usize {
//     let mut num:usize = 0;
//     unsafe{std::ptr::copy_nonoverlapping(input, &mut num as *mut usize as *mut u8, 8);} 
//     num
// }
#[inline]
fn read_u32_ptr(input: *const u8) -> u32 {
    let mut num:u32 = 0;
    unsafe{std::ptr::copy_nonoverlapping(input, &mut num as *mut u32 as *mut u8, 4);} 
    num
}
#[inline]
fn read_usize_ptr(input: *const u8) -> usize {
    let mut num:usize = 0;
    unsafe{std::ptr::copy_nonoverlapping(input, &mut num as *mut usize as *mut u8, std::mem::size_of::<usize>());} 
    num
}
#[inline]
fn read_u16_ptr(input: *const u8) -> u16 {
    let mut num:u16 = 0;
    unsafe{std::ptr::copy_nonoverlapping(input, &mut num as *mut u16 as *mut u8, 2);} 
    num
}
// #[inline]
// fn read_u64(input: &[u8]) -> usize {
//     let mut num:usize = 0;
//     unsafe{std::ptr::copy_nonoverlapping(input.as_ptr(), &mut num as *mut usize as *mut u8, 8);} 
//     num
// }
// fn read_u32(input: &[u8]) -> u32 {
//     let mut num:u32 = 0;
//     unsafe{std::ptr::copy_nonoverlapping(input.as_ptr(), &mut num as *mut u32 as *mut u8, 4);} 
//     num
// }

// fn read_u16(input: &[u8]) -> u16 {
//     let mut num:u16 = 0;
//     unsafe{std::ptr::copy_nonoverlapping(input.as_ptr(), &mut num as *mut u16 as *mut u8, 2);} 
//     num
// }

#[inline]
fn get_common_bytes(diff: usize) -> u32 {
    let tr_zeroes = diff.trailing_zeros();
    // right shift by 3, because we are only interested in 8 bit blocks
    tr_zeroes >> 3
}