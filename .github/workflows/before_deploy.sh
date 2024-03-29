set -ex

main() {
    local tag=$(git tag --points-at HEAD)
    local src=$(pwd) \
          stage=

    if [ "$OS_NAME" = "macOS-latest" ]; then
        stage=$(mktemp -d -t tmp)
    else
        stage=$(mktemp -d)
    fi

    if [[ "$OS_NAME" = ubuntu-* ]]; then
        cp target/$TARGET/max-opt/asr-debugger $stage/.
    elif [ "$OS_NAME" = "macOS-latest" ]; then
        cp target/$TARGET/max-opt/asr-debugger $stage/.
    elif [ "$OS_NAME" = "windows-latest" ]; then
        cp target/$TARGET/max-opt/asr-debugger.exe $stage/.
    fi

    cd $stage
    if [ "$OS_NAME" = "windows-latest" ]; then
        7z a $src/asr-debugger-$tag-$TARGET.zip *
    else
        tar czf $src/asr-debugger-$tag-$TARGET.tar.gz *
    fi
    cd $src

    rm -rf $stage
}

main
