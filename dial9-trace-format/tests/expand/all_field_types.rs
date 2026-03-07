use dial9_trace_format::{InternedString, StackFrames, TraceEvent};

#[derive(TraceEvent)]
struct AllFieldTypes {
    a_u8: u8,
    b_u16: u16,
    c_u32: u32,
    d_u64: u64,
    e_i64: i64,
    f_f64: f64,
    g_bool: bool,
    h_string: String,
    i_bytes: Vec<u8>,
    j_interned: InternedString,
    k_frames: StackFrames,
    l_map: Vec<(String, String)>,
}

fn main() {}
