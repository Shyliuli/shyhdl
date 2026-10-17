mod a;

#[hardware_instance]
struct CycleChild {
    #[In] p: bool,
    #[Out] q: bool,
}
impl Hardware for CycleChild {
    fn run(&mut self) {
        self.q = self.p;
    }
}
