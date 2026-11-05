# Python SDK releases

This fork does not publish Python runtime or SDK packages to PyPI. The upstream
`python-runtime-release`, `python-sdk-cli-release`, and `python-sdk-release`
workflows and their release-resolution helpers have been removed.

The runtime wheel is still produced as part of the Rust release build, so local
packaging and SDK smoke tests continue to work without the publication workflows.
