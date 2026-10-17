// ShyHDL acceptance fixture for fixed-length arrays. Compiled by
// `frontend::compile(source, name, "ArrTop")`; never compiled by rustc.
//
// Layout, which every expectation in `tb_arrays.v` is built from by hand:
//
// * `[T; N]` packs element 0 into the least significant bits, so element 1 is
//   the next slice up and so on,
// * a struct packs its fields most significant first, in declaration order.
//
// The two rules compose, so `[Cell; 2]` puts cell 0 in the low byte and
// `struct Bundle { items: [uint<4>; 2], flag: bool }` puts `items` above `flag`
// with item 0 in the low nibble of its field.
//
// `ArrConst` covers the literals, the constant element write, the whole-array
// copy and the half-open sub-array; `ArrDynRead` a dynamic uint index read and
// the unknown an out-of-range index produces; `ArrDynWrite` a combinational
// dynamic write that needs a whole-array default, and a sequential one whose
// non-blocking write leaves the other elements on their old value; `ArrNest`
// an array of signed-field structs and a struct holding an array.

#[hardware_instance]
struct ArrConst {
    #[In] v: uint<4>,
    #[In] k: uint<1>,
    #[In] m: uint<2>,
    #[Out] repeated: [uint<4>; 3],
    #[Out] listed: [uint<4>; 3],
    #[Out] copied: [uint<4>; 3],
    #[Out] sub: [uint<4>; 2],
    #[Out] elem0: uint<4>,
    #[Out] elem2: uint<4>,
    #[Out] nested: [[uint<4>; 2]; 2],
    #[Out] nested_elem: uint<4>,
    #[Out] nested_dyn: uint<4>,
}

impl Hardware for ArrConst {
    fn run(&mut self) {
        // A repeat literal, then a list literal written element 0 first.
        self.repeated = [self.v; 3];
        self.listed = [self.v, 0x1, 0x2];
        // A constant element write, then a whole-array copy of the same shape.
        self.listed[1] = 0x7;
        self.copied = self.listed;
        // A half-open sub-array, and constant element reads. A sub-array is
        // spelled with the method, since a bracket range means raw bits.
        self.sub = self.copied.slice(0, 2);
        self.elem0 = self.copied[0];
        self.elem2 = self.copied[2];

        // An element type that is itself an array: a nested literal whose inner
        // element repeats, a nested constant index, and a nested dynamic inner
        // index. An inner index with no element reads the unknown instead of
        // spilling into the element beside it.
        self.nested = [[self.v; 2], [0x2, 0x3]];
        self.nested_elem = self.nested[1][0];
        self.nested_dyn = self.nested[self.k][self.m];
    }
}

#[hardware_instance]
struct ArrDynRead {
    #[In] src: [uint<4>; 3],
    #[In] i: uint<2>,
    #[In] one: [bool; 1],
    #[In] j: uint<1>,
    #[In] j3: uint<3>,
    #[Out] y: uint<4>,
    #[Out] one_bit: bool,
    #[Out] bit_from_element: bool,
    #[Out] cast_y: uint<4>,
}

impl Hardware for ArrDynRead {
    fn run(&mut self) {
        // Index 3 has no element, so this reads the standard unknown.
        self.y = self.src[self.i];
        // A one-element array: index 0 is the element, index 1 has none and
        // reads the unknown.
        self.one_bit = self.one[self.j];
        // `src[0]` is a four-bit element, and this index then selects one bit of
        // it. The bit select must apply to that element, not to the whole array,
        // and an index past the element's own width has to read the unknown
        // instead of spilling into the neighbouring element.
        self.bit_from_element = self.src[0][self.j3];

        // The same read through a signed index value: it is cast back to an
        // unsigned type before use, so the element it selects is the same one
        // `src[self.i]` selects, and a negative-looking bit pattern must not be
        // treated as a negative offset.
        let signed_index: int<2> = self.i as int<2>;
        self.cast_y = self.src[signed_index as uint<2>];
    }
}

#[hardware_instance]
struct ArrDynWrite {
    #[In] v: uint<4>,
    #[In] i: uint<2>,
    #[In] oi: uint<2>,
    #[In] ii: uint<2>,
    #[In] fi: uint<2>,
    #[In] clk: bool,
    #[In] rst_n: bool,
    #[Out] comb_out: [uint<4>; 3],
    #[Out] comb_read: uint<4>,
    #[Out] seq_out: [uint<4>; 3],
    #[Out] snap: uint<4>,
    #[Out] nest_out: [[uint<4>; 2]; 2],
    #[Out] field_out: [Cell; 2],
}

impl Hardware for ArrDynWrite {
    fn run(&mut self) {
        // A combinational dynamic write needs every element driven first, so the
        // whole array is defaulted and the write lands on top of it. An index
        // with no element changes nothing.
        self.comb_out = [0x1, 0x2, 0x3];
        self.comb_out[self.i] = self.v;
        // Reading the array back in the same process, after the write, is what
        // shows the source order: this element carries the value just written,
        // not the default it had one statement earlier.
        self.comb_read = self.comb_out[self.i];

        // A nested dynamic write: the whole array is defaulted first, then the
        // selected inner element changes. An inner index with no element leaves
        // even its own element at the default, and an outer index with no
        // element leaves the whole array at the default.
        self.nest_out = [[0x1, 0x2], [0x3, 0x4]];
        self.nest_out[self.oi][self.ii] = self.v;

        // A dynamic element write that replaces one field of the selected
        // struct: the other field and the struct beside it keep the default, and
        // an index with no element changes nothing at all.
        self.field_out = [Cell { tag: 0x1, val: 0x2 }, Cell { tag: 0x3, val: 0x4 }];
        self.field_out[self.fi].val = self.v;

        always! {
            if negedge(self.rst_n) {
                self.seq_out = [0; 3];
                self.snap = 0;
            } else if posedge(self.clk) {
                // The same element is read and written at one edge. Both are
                // non-blocking, so the read gets the value from before the edge,
                // which is what proves the write did not take effect early; the
                // elements this index does not select keep their old value, and
                // an out-of-range index keeps all of them.
                self.snap = self.seq_out[self.i];
                self.seq_out[self.i] = self.v;
            }
        }
    }
}

/// A struct whose tag is signed, so an array of them also has to keep signedness
/// through the element index.
struct Cell {
    tag: int<4>,
    val: uint<4>,
}

/// A struct whose first field is an array.
struct Bundle {
    items: [uint<4>; 2],
    flag: bool,
}

#[hardware_instance]
struct ArrNest {
    #[In] cells: [Cell; 2],
    #[In] b: Bundle,
    #[Out] tags: [int<4>; 2],
    #[Out] vals: [uint<4>; 2],
    #[Out] tag0_ext: int<8>,
    #[Out] b0: uint<4>,
    #[Out] b1: uint<4>,
    #[Out] bflag: bool,
    #[Out] cell0: Cell,
}

impl Hardware for ArrNest {
    fn run(&mut self) {
        self.tags = [self.cells[0].tag, self.cells[1].tag];
        self.vals = [self.cells[0].val, self.cells[1].val];
        // The tag is signed, so sign-extending it out of the array keeps the
        // sign rather than zero-filling the high bits.
        self.tag0_ext = self.tags[0].sext::<8>();

        self.b0 = self.b.items[0];
        self.b1 = self.b.items[1];
        self.bflag = self.b.flag;

        // A whole struct element, copied across an array index.
        self.cell0 = self.cells[0];
    }
}

/// Passes every leaf through one level of hierarchy, so an array and a struct
/// holding one also have to survive a module boundary.
#[hardware_instance]
struct ArrTop {
    #[In] v: uint<4>,
    #[In] i: uint<2>,
    #[In] k: uint<1>,
    #[In] m: uint<2>,
    #[In] j: uint<1>,
    #[In] j3: uint<3>,
    #[In] one: [bool; 1],
    #[In] oi: uint<2>,
    #[In] ii: uint<2>,
    #[In] fi: uint<2>,
    #[In] src: [uint<4>; 3],
    #[In] cells: [Cell; 2],
    #[In] b: Bundle,
    #[In] clk: bool,
    #[In] rst_n: bool,
    #[Out] repeated: [uint<4>; 3],
    #[Out] listed: [uint<4>; 3],
    #[Out] copied: [uint<4>; 3],
    #[Out] sub: [uint<4>; 2],
    #[Out] elem0: uint<4>,
    #[Out] elem2: uint<4>,
    #[Out] nested: [[uint<4>; 2]; 2],
    #[Out] nested_elem: uint<4>,
    #[Out] nested_dyn: uint<4>,
    #[Out] y: uint<4>,
    #[Out] one_bit: bool,
    #[Out] bit_from_element: bool,
    #[Out] cast_y: uint<4>,
    #[Out] comb_out: [uint<4>; 3],
    #[Out] comb_read: uint<4>,
    #[Out] seq_out: [uint<4>; 3],
    #[Out] snap: uint<4>,
    #[Out] nest_out: [[uint<4>; 2]; 2],
    #[Out] field_out: [Cell; 2],
    #[Out] tags: [int<4>; 2],
    #[Out] vals: [uint<4>; 2],
    #[Out] tag0_ext: int<8>,
    #[Out] b0: uint<4>,
    #[Out] b1: uint<4>,
    #[Out] bflag: bool,
    #[Out] cell0: Cell,
    #[instance] konst: ArrConst,
    #[instance] reader: ArrDynRead,
    #[instance] writer: ArrDynWrite,
    #[instance] nest: ArrNest,
}

impl Hardware for ArrTop {
    fn run(&mut self) {
        self.konst.v = self.v;
        self.konst.k = self.k;
        self.konst.m = self.m;
        self.repeated = self.konst.repeated;
        self.listed = self.konst.listed;
        self.copied = self.konst.copied;
        self.sub = self.konst.sub;
        self.elem0 = self.konst.elem0;
        self.elem2 = self.konst.elem2;
        self.nested = self.konst.nested;
        self.nested_elem = self.konst.nested_elem;
        self.nested_dyn = self.konst.nested_dyn;

        self.reader.src = self.src;
        self.reader.i = self.i;
        self.reader.one = self.one;
        self.reader.j = self.j;
        self.reader.j3 = self.j3;
        self.y = self.reader.y;
        self.one_bit = self.reader.one_bit;
        self.bit_from_element = self.reader.bit_from_element;
        self.cast_y = self.reader.cast_y;

        self.writer.v = self.v;
        self.writer.i = self.i;
        self.writer.oi = self.oi;
        self.writer.ii = self.ii;
        self.writer.fi = self.fi;
        self.writer.clk = self.clk;
        self.writer.rst_n = self.rst_n;
        self.comb_out = self.writer.comb_out;
        self.comb_read = self.writer.comb_read;
        self.seq_out = self.writer.seq_out;
        self.snap = self.writer.snap;
        self.nest_out = self.writer.nest_out;
        self.field_out = self.writer.field_out;

        self.nest.cells = self.cells;
        self.nest.b = self.b;
        self.tags = self.nest.tags;
        self.vals = self.nest.vals;
        self.tag0_ext = self.nest.tag0_ext;
        self.b0 = self.nest.b0;
        self.b1 = self.nest.b1;
        self.bflag = self.nest.bflag;
        self.cell0 = self.nest.cell0;
    }
}
