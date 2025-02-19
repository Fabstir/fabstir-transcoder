#!/bin/bash

echo 'if [ -d "/usr/local/cuda-12.8.0/bin" ] ; then
    PATH="/usr/local/cuda-12.8.0/bin:$PATH"
fi

if [ -d "/usr/local/cuda-12.8.0/lib64" ] ; then
    LD_LIBRARY_PATH="/usr/local/cuda-12.8.0/lib64:$LD_LIBRARY_PATH"
fi

if [ -d "/usr/local/lib" ] ; then
    LD_LIBRARY_PATH="/usr/local/lib:$LD_LIBRARY_PATH"
fi

LD_LIBRARY_PATH=/usr/local/bin:$LD_LIBRARY_PATH
' >> ~/.profile

source ~/.profile