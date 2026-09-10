# snapcompact-cli

Rasterize text into snapcompact pixel-font PNG frames.

## Usage

```
snapcompact [TEXT] [OPTIONS]
```

With no `TEXT`, the tool reads all of stdin.

## Options

| Flag | Description | Default |
|---|---|---|
| `--shape <SHAPE>` | Frame shape | `11on16-bw` |
| `--size <N>` | Frame edge in pixels (`1`..=`16384`); the bitmap is `N` wide and the height hugs the rows the text uses | `1568` |
| `-o, --output <PATH>` | Output PNG path; when omitted, raw PNG bytes go to stdout | |
| `--dim` | Enable stopword dimming (gray ink); already on for the `6x12-dim` shape | |
| `--version` | Print version | |

## Shapes

| Shape | Font | Geometry |
|---|---|---|
| `11on16-bw` | 8x13 BDF | 11×16 px cell |
| `8on22-bw` | 8x13 BDF | 8×22 px cell |
| `8on16-bw` | 8x13 BDF | 8×16 px cell |
| `6x12-dim` | 6x12 BDF | 6×12 px cell (stopword dim) |
| `silver16-bw` | Silver TTF | 16×16 px cell |

Bitmap shapes fall back to the Silver TTF for wide characters (CJK and similar) when `fonts/Silver.ttf` is present.

## Examples

```sh
snapcompact "the quick brown fox" -o fox.png
snapcompact "hello world" --shape 8on22-bw --size 960 -o hello.png
echo "piped text" | snapcompact
```

## Build

```sh
cargo build --release
# binary at target/release/snapcompact
```

## Fonts

The 8x13 BDF font is compiled into the binary. The `6x12` and `silver` shapes load from a `fonts/` directory in the current working directory at run time: `6x12-dim` requires `fonts/6x12.bdf`, `silver16-bw` requires `fonts/Silver.ttf`. The bundled `fonts/` directory ships the BDF and TTF font files.

## Credits

`snapcompact-cli` is a standalone Rust port of the PNG rasterizer from
[oh-my-pi](https://github.com/can1357/oh-my-pi) by [Can Bölük](https://github.com/can1357).
The bitmap-font rendering pipeline (BDF glyph blitting, indexed PNG encoding with
palette narrowing) replicates the original implementation in the `pi-natives` Rust
N-API addon (MIT licensed). BDF font files (`8x13.bdf`, `6x12.bdf`) are sourced
from the oh-my-pi repository.
