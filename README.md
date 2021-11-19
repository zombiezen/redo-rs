# zombiezen/redo

This is a Rust port of [Avery Pennarun's redo][], which in turn is an
implementation of [Daniel J. Bernstein's redo build system][]. It includes
parallel builds, improved logging, extensive automated tests, and helpful
debugging features.

[Avery Pennarun's redo]: https://github.com/apenwarr/redo
[Daniel J. Bernstein's redo build system]: http://cr.yp.to/redo.html

## Installation

1. Download the binary for your platform from the [latest release][].
2. Unzip the archive.
3. Move the contents of the `bin/` directory into a directory in your `PATH`.

If you use Visual Studio Code, you may be interested in installing the [redo extension][].

[latest release]: https://github.com/zombiezen/redo-rs/releases/latest
[redo extension]: https://marketplace.visualstudio.com/items?itemName=zombiezen.redo

## Getting Started

_Borrowed from https://redo.readthedocs.io/en/latest/cookbook/hello/_

Create a source file:

```shell
cat > hello.c <<EOF
#include <stdio.h>

int main() {
    printf("Hello, world!\n");
    return 0;
}
EOF
```

Create a .do file to tell redo how to compile it:

```shell
cat > hello.do <<EOF
# If hello.c changes, this script needs to be
# re-run.
redo-ifchange hello.c

# Compile hello.c into the 'hello' binary.
#
# $3 is the redo variable that represents the
# output filename.  We want to build a file
# called "hello", but if we write that directly,
# then an interruption could result in a
# partially-written file.  Instead, write it to
# $3, and redo will move our output into its
# final location, only if this script completes
# successfully.
#
cc -o $3 hello.c -Wall
EOF
```

Build and run the program:

```shell
redo hello &&
./hello
```

You can provide a default target by writing an `all.do` file:

```shell
echo 'redo-ifchange hello' > all.do &&
redo &&
./hello
```

## Further Reading

The extensive [documentation for Avery Pennarun's redo](https://redo.readthedocs.io/en/latest/)
is relevant for this tool as well, since zombiezen/redo is a straight port.
