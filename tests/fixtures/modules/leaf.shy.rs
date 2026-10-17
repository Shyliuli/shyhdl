// Loaded by `mod leaf;` in `top.shy.rs`. It pulls in one more file of its own,
// so the loader has to follow `mod` recursively.

mod tag;

#[hardware_instance]
struct LeafAdder {
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    #[Out] sum: uint<8>,
}
impl Hardware for LeafAdder {
    fn run(&mut self) {
        self.sum = self.a.zext::<8>() + self.b.zext::<8>();
    }
}

#[hardware_instance]
struct Tagger {
    #[In] value: uint<8>,
    #[Out] flag: bool,
}
impl Hardware for Tagger {
    fn run(&mut self) {
        // A plain data struct from `tag.shy.rs`: visible without a module path,
        // because every file joins one namespace.
        let t: LeafTag = LeafTag { set: self.value.get(0) };
        self.flag = t.set;
    }
}
