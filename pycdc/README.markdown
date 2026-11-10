# Decompyle++ 
***A Python Byte-code Disassembler/Decompiler***

Decompyle++ aims to translate compiled Python byte-code back into valid
and human-readable Python source code. While other projects have achieved
this with varied success, Decompyle++ is unique in that it seeks to
support byte-code from any version of Python.

Decompyle++ includes both a byte-code disassembler (pycdas) and a 
decompiler (pycdc).

As the name implies, Decompyle++ is written in C++.
If you wish to contribute, please fork us on github at 
https://github.com/zrax/pycdc

## Building Decompyle++
* Generate a project or makefile with [CMake](http://www.cmake.org) (See CMake's documentation for details)
  * The following options can be passed to CMake to control debug features:

    | Option | Description |
    | --- | --- |
    | `-DCMAKE_BUILD_TYPE=Debug` | Produce debugging symbols |
    | `-DENABLE_BLOCK_DEBUG=ON` | Enable block debugging output |
    | `-DENABLE_STACK_DEBUG=ON` | Enable stack debugging output |

* Build the generated project or makefile
  * For projects (e.g. MSVC), open the generated project file and build it
  * For makefiles, just run `make`
  * To run tests (on \*nix or MSYS), run `make check JOBS=4` (optional
    `FILTER=xxxx` to run only certain tests)

## Usage
**To run pycdas**, the PYC Disassembler:
`./pycdas [PATH TO PYC FILE]`
The byte-code disassembly is printed to stdout.

**To run pycdc**, the PYC Decompiler: 
`./pycdc [PATH TO PYC FILE]`
The decompiled Python source is printed to stdout.
Any errors are printed to stderr.

A whole tree can be decompiled at once by passing a directory; every `*.pyc`
below it is decompiled:
`./pycdc [PATH TO DIRECTORY] -o [OUTPUT DIRECTORY]`

With `-o`, the argument is treated as a directory unless it ends in `.py` (or
already is one).  Files are then written to the source path recorded inside
 each module (`co_filename`), so a tree of renamed bytecode files is restored
to its original layout:
`./pycdc extracted/7eda1a8b_6ce325b5.pyc -o output`
writes `output/gui_2d/ui/hall/developer/view/DeveloperIntroductionWindow.py`.
That path is also reported in the header of every decompiled file, as
`# Embedded file name:`.

Progress is written to stderr, one line per finished file, so stdout stays
clean when it is used for decompiled source:
`[3/4354] -> output/gui_2d/ui/hall/developer/view/DeveloperIntroductionWindow.py`

**Marshalled code objects**:
Both tools support Python marshalled code objects, as output from `marshal.dumps(compile(...))`.

To use this feature, specify `-c -v <version>` on the command line - the version must be specified as the objects themselves do not contain version metadata.

## Authors, Licence, Credits
Decompyle++ is the work of Michael Hansen and Darryl Pogue.

Additional contributions from:
* charlietang98
* Kunal Parmar
* Olivier Iffrig
* Zlodiy

It is released under the terms of the GNU General Public License, version 3;
See LICENSE file for details.
