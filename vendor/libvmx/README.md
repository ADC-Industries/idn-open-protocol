# libvmx (vendored)

The VMX codec of Open Media Transport, from https://github.com/openmediatransport/libvmx
at commit 31724bb96530325a989bc157c8e07538d6f1c4db (2026-10-20), MIT licence
(LICENSE.txt). Only its sources, unchanged; `core/idn-omt/build.rs` compiles them (the
AVX2 file alone with AVX2 flags: the codec chooses that path at run time). Not built:
the Visual Studio project, `dllmain.cpp`, the build scripts.
