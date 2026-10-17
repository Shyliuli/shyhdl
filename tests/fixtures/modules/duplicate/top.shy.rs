// The same struct name in two files: one design namespace cannot hold both.
mod one;
mod two;

#[hardware_instance]
struct DuplicateTop {
    #[In] a: bool,
    #[Out] y: bool,
    #[instance] kid: SharedName,
}
impl Hardware for DuplicateTop {
    fn run(&mut self) {
        self.kid.a = self.a;
        self.y = self.kid.y;
    }
}
