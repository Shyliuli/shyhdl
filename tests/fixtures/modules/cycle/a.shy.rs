// `a` loads `b`, and `b` loads `a` again: a cycle, not a diamond.
mod b;

#[hardware_instance]
struct CycleTop {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for CycleTop {
    fn run(&mut self) {
        self.y = self.a;
    }
}
