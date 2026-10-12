use opcode_router::route_methods;

struct Handler;

#[route_methods(id_type = u16)]
impl Handler {
    #[route]
    fn missing(&mut self) {}
}

fn main() {}
