# src/prng_shim

クレート`higanr_prng_shim`。Rustの標準ライブラリは、`HashMap`の乱数の種を作るときに`bcryptprimitives.dll`の`ProcessPrng`を通常のimportで呼びます。
そのため、古いWine（DLLが無い・`ProcessPrng`が無い・あっても呼ぶと中断する穴埋め）では、HiganRTool.exeとHiganRTool.dllが動き出す前後に落ちます。これを避けるための部品です。
`src/loader`のHiganRTool.exeと`src/dll`のHiganRTool.dllの両方が使います（`unsafe`が要るので`src/core`には置けません）。

- 各クレートの`build.rs`が`/DELAYLOAD:`（DLLの名前は`PRNG_DLL`）と`delayimp.lib`をリンカーに渡し、`ProcessPrng`を最初に呼ぶときまで読み込みを遅らせます。
- DLLが読めないときと関数が無いときは、失敗の遅延読み込みのhook（`__pfnDliFailureHook2`）が、`ProcessPrng`の代わりに`process_prng_fallback`を遅延読み込みの処理へ返します。
- 遅延読み込みの処理が`ProcessPrng`のアドレスを求めたとき、Wineの上でだけ、通知のhook（`__pfnDliNotifyHook2`）が先に`process_prng_fallback`を返します（穴埋めの関数は`GetProcAddress`が成功するので失敗のhookに来ないため）。
- 代わりの`process_prng_fallback`は、`advapi32.dll`の`SystemFunction036`（RtlGenRandom）を呼びます。
- hookの変数は`install_delay_load_hook!`で使う側のクレートの中に置きます（リンカーが確実に自分の定義を選ぶため）。検査は`assert_delay_load_hooks_installed!`。
- Windowsで普通に動くときは本物の`ProcessPrng`が使われます。

検査: `cargo test -p higanr_prng_shim`。判定の単体の検査と、本物の遅延読み込み（`tests/delay_load.rs`）で4つの形（DLLが無い・関数が無い・関数があってWineの上・関数があってWineでない）を検証する検査があります。
