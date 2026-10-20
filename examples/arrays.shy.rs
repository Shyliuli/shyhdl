//! Fixed-length arrays: `[T; N]`, element 0 in the least significant bits.
//!
//! Build it with `shyhdl build --top ArrayBank examples/arrays.shy.rs`.
//!
//! An array is a value with a fixed layout, never memory: `[T; N]` puts element 0
//! in the lowest bits and the next element above it, and a struct packs its
//! fields most significant first, so the two rules compose. A build-time index
//! selects an element, a range selects a sub-array, and a run-time index selects
//! one element too - an index with no element reads the standard unknown and
//! writes nothing at all. Nothing here is inferred as a RAM.

/// A struct holding an array: `items` sits above `flag`, with item 0 in the low
/// half of `items`.
struct Bundle {
    items: [uint<4>; 2],
    flag: bool,
}

#[hardware_instance]
struct ArrayBank {
    #[In] d: uint<8>,
    #[In] i: uint<2>,
    #[Out] filled: [uint<8>; 3],
    #[Out] listed: [uint<8>; 3],
    #[Out] pair: [uint<8>; 2],
    #[Out] picked: uint<8>,
    #[Out] written: [uint<8>; 3],
    #[Out] readback: uint<8>,
    #[Out] low: uint<4>,
    #[Out] high: uint<4>,
    #[Out] flag: bool,
}

impl Hardware for ArrayBank {
    fn run(&mut self) {
        // A repeat literal, and a list literal written element 0 first.
        self.filled = [self.d; 3];
        self.listed = [self.d, 0x1, 0x2];
        // A single index selects one element; `.slice` selects a sub-array.
        self.pair = self.listed.slice(0, 2);
        self.picked = self.listed[2];

        // A run-time index writes one element, so the whole array is driven
        // first: the elements the index does not select keep the default, and
        // would otherwise have to be remembered - a latch, not a register.
        self.written = [0x10, 0x20, 0x30];
        self.written[self.i] = self.d;
        // Reading the array back in the same process sees what the write did.
        self.readback = self.written[self.i];

        // A struct holding an array: its fields are ordinary reads.
        let bundle: Bundle = Bundle {
            items: [self.d[0..4], self.d[4..8]],
            flag: self.d.get(7),
        };
        self.low = bundle.items[0];
        self.high = bundle.items[1];
        self.flag = bundle.flag;
    }
}
