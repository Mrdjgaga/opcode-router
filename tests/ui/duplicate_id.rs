use opcode_router::route_methods;

struct Handler;

#[route_methods(id_type = u16)]
impl Handler {
    #[route(id = 7u16)]
    fn first(&mut self) {}

    #[route(id = 7u16)]
    fn second(&mut self) {}
}

fn main() {}
