use opcode_router::route_methods;

struct Handler;

#[route_methods(id_type = u16)]
impl Handler {
    #[route(id = 1u16 + 2u16)]
    fn invalid(&mut self) {}
}

fn main() {}
