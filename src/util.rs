pub trait Pipe: Sized {
    fn pipe<B, F>(self, f: F) -> B
    where
        F: FnOnce(Self) -> B,
    {
        f(self)
    }
}

impl<T: Sized> Pipe for T {} 