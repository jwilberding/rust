
import size_of = std.sys.rustrt.size_of;

use std;

fn main() {
  check (size_of[u8]() == (1 as uint));
  check (size_of[u32]() == (4 as uint));
  check (size_of[char]() == (4 as uint));
  check (size_of[i8]() == (1 as uint));
  check (size_of[i32]() == (4 as uint));
  check (size_of[tup(u8,i8)]() == (2 as uint));
  check (size_of[tup(u8,i8,u8)]() == (3 as uint));
  // Alignment causes padding before the char and the u32.
  check (size_of[tup(u8,i8,tup(char,u8),u32)]() == (16 as uint));
  check (size_of[int]() == size_of[uint]());
  check (size_of[tup(int,())]() == size_of[int]());
  check (size_of[tup(int,(),())]() == size_of[int]());
  check (size_of[int]() == size_of[rec(int x)]());
}
