#[hardware_instance]
struct SharedName {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for SharedName {
    fn run(&mut self) {
        self.y = !self.a;
    }
}
