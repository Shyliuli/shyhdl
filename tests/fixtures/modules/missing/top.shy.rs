// `mod ghost;` has no file next to this one: the loader must report it.
mod ghost;

#[hardware_instance]
struct MissingTop {
    #[In] a: bool,
    #[Out] y: bool,
}
impl Hardware for MissingTop {
    fn run(&mut self) {
        self.y = self.a;
    }
}
