// A design split over two files: `mod leaf;` loads `leaf.shy.rs` from this
// directory, and every item it declares joins this design's namespace.
mod leaf;

#[hardware_instance]
struct TourTop {
    #[In] a: uint<4>,
    #[In] b: uint<4>,
    #[Out] y: uint<8>,
    #[Out] flagged: bool,
    #[instance] adder: LeafAdder,
    #[instance] tagger: Tagger,
}
impl Hardware for TourTop {
    fn run(&mut self) {
        self.adder.a = self.a;
        self.adder.b = self.b;
        self.tagger.value = self.adder.sum;
        self.y = self.adder.sum;
        self.flagged = self.tagger.flag;
    }
}
