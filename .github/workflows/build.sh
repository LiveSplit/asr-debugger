set -ex

main() {
    local cargo=cross
    if [ "$SKIP_CROSS" = "skip" ]; then
        cargo=cargo
    fi
    local release_flag=""
    local target_folder="debug"
    if [ "$IS_DEPLOY" = "true" ]; then
        release_flag="--profile max-opt"
        target_folder="max-opt"
    fi

    $cargo build --target $TARGET $release_flag $FEATURES
}

main
