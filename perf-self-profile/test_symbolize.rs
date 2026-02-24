use perf_self_profile::resolve_symbol;

#[inline(never)]
fn test_function() -> u64 {
    42
}

fn main() {
    let func_addr = test_function as u64;
    println!("Function address: {:#x}", func_addr);
    
    let sym = resolve_symbol(func_addr);
    println!("Symbol name: {:?}", sym.name);
    println!("Base addr: {:#x}", sym.base_addr);
    println!("Object: {:?}", sym.object);
    println!("Offset: {:#x}", sym.offset);
    
    // Also test with the actual address from execution
    let result = test_function();
    println!("\nResult: {}", result);
}
