# Third-party notices

This file records source-derived material and nontrivial runtime dependency
obligations for glu. The generated `THIRD_PARTY_LICENSES.html` file contains
the complete license texts and crate-to-license mapping for the locked macOS
release dependency graph.

## Homebrew-derived source

Parts of `glu-client` are adapted from or intentionally reproduce implementation
behavior from [Homebrew/brew](https://github.com/Homebrew/brew), especially:

- structured postinstall step execution and formula actions;
- postinstall sandboxing and environment handling;
- keg linking, unlinking, overwrite, and `install_info` policy;
- bottle and Mach-O relocation behavior.

The structured postinstall source contains more detailed citations because
those comments are also used to track upstream semantic changes. Other
subsystems are attributed here on a best-effort basis rather than through an
exhaustive file-by-file provenance map. Existing source citations record the
representative revisions used for compatibility review.

Homebrew/brew and Homebrew/homebrew-core are distributed under the BSD
2-Clause License. The applicable copyright notice and license are reproduced
in `LICENSE-BSD-2-Clause`.

## ruby-macho-derived source

Mach-O parsing, relocation, load-command growth, fat-archive handling, and
code-signing identifier behavior were informed by or adapted from
[Homebrew/ruby-macho](https://github.com/Homebrew/ruby-macho).

ruby-macho is distributed under the MIT License:

> The MIT License (MIT)
>
> Copyright (c) 2015, 2016, 2017, 2018 William Woodruff
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

## Dagre

The self-contained trace viewer includes Dagre 0.8.5, distributed under the
MIT License:

> Copyright (c) 2012-2014 Chris Pettitt
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
> THE SOFTWARE.

The exact vendored Dagre license is also retained at
`crates/glu-client/assets/dagre-0.8.5.LICENSE`.

The Dagre distribution bundles Graphlib 2.1.8. Its distribution contains the
following BSD 3-Clause notice:

> Copyright (c) 2014, Chris Pettitt
> All rights reserved.
>
> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are met:
>
> 1. Redistributions of source code must retain the above copyright notice,
> this list of conditions and the following disclaimer.
>
> 2. Redistributions in binary form must reproduce the above copyright notice,
> this list of conditions and the following disclaimer in the documentation
> and/or other materials provided with the distribution.
>
> 3. Neither the name of the copyright holder nor the names of its contributors
> may be used to endorse or promote products derived from this software without
> specific prior written permission.
>
> THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
> AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
> IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
> ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
> LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
> CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
> SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
> INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
> CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
> ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
> POSSIBILITY OF SUCH DAMAGE.

The distribution also bundles Lodash 4.x modules under the MIT License:

> Copyright OpenJS Foundation and other contributors
>
> Based on Underscore.js, copyright Jeremy Ashkenas, DocumentCloud and
> Investigative Reporters & Editors
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
> THE SOFTWARE.

## MPL-2.0 runtime components

The `0.1.3` release binary includes unmodified components distributed under
MPL-2.0. Their complete license text is included in
`THIRD_PARTY_LICENSES.html`. The exact Source Code Form for each locked version
is available from crates.io:

| Component | Version | Exact source archive |
|---|---:|---|
| apple-bundles | 0.21.0 | <https://crates.io/api/v1/crates/apple-bundles/0.21.0/download> |
| apple-codesign | 0.29.0 | <https://crates.io/api/v1/crates/apple-codesign/0.29.0/download> |
| apple-flat-package | 0.20.0 | <https://crates.io/api/v1/crates/apple-flat-package/0.20.0/download> |
| apple-xar | 0.20.0 | <https://crates.io/api/v1/crates/apple-xar/0.20.0/download> |
| cpio-archive | 0.10.0 | <https://crates.io/api/v1/crates/cpio-archive/0.10.0/download> |
| cryptographic-message-syntax | 0.27.0 | <https://crates.io/api/v1/crates/cryptographic-message-syntax/0.27.0/download> |
| option-ext | 0.2.0 | <https://crates.io/api/v1/crates/option-ext/0.2.0/download> |
| x509-certificate | 0.24.0 | <https://crates.io/api/v1/crates/x509-certificate/0.24.0/download> |

These components remain under MPL-2.0. Their inclusion does not change the
license of glu's independently licensed source files.

## Non-affiliation

glu is an independent project. It is not affiliated with, maintained by,
sponsored by, or endorsed by Homebrew or Apple Inc. Homebrew is a third-party
project and trademark belonging to its respective owners.
