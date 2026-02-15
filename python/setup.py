"""Build the btelem._native C extension."""

from setuptools import setup, Extension
import numpy

setup(
    ext_modules=[
        Extension(
            "btelem._native",
            sources=["btelem/_native.c"],
            include_dirs=["../include", numpy.get_include()],
            extra_compile_args=["-std=c99", "-O2", "-Wall"],
        ),
    ],
)
