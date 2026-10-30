// Run from the native Celo Go module. Arguments: RPC URL, output JSON.
// The archive headers and eth_call results come from the same native node.
package main

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/celo-org/celo-blockchain/common/hexutil"
	"github.com/celo-org/celo-blockchain/core/types"
	"github.com/celo-org/celo-blockchain/rlp"
	"github.com/celo-org/celo-blockchain/rpc"
)

type headerVector struct {
	Number uint64 `json:"number"`
	Hash   string `json:"hash"`
	RLP    string `json:"rlp"`
}
type callVector struct {
	Name     string `json:"name"`
	Block    uint64 `json:"block"`
	Address  uint8  `json:"address"`
	Input    string `json:"input"`
	Expected string `json:"expected"`
	Error    bool   `json:"error"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Minute)
	defer cancel()
	client, err := rpc.DialContext(ctx, os.Args[1])
	check(err)
	defer client.Close()
	const latest uint64 = 31_056_499
	samples := []uint64{1, 17280, 17281, 6773999, 6774000, 11838439, 11838440, 21615999, 21616000, 31056496, latest}
	heights := make([]uint64, 0, 1900)
	for n := uint64(0); n < latest; n += 17280 {
		heights = append(heights, n)
	}
	heights = append(heights, samples...)
	headers := make([]headerVector, 0, len(heights))
	encoded := make(map[uint64]string)
	for start := 0; start < len(heights); start += 32 {
		end := start + 32
		if end > len(heights) {
			end = len(heights)
		}
		raw := make([]json.RawMessage, end-start)
		batch := make([]rpc.BatchElem, end-start)
		for i, n := range heights[start:end] {
			batch[i] = rpc.BatchElem{Method: "eth_getBlockByNumber", Args: []interface{}{hexutil.EncodeUint64(n), false}, Result: &raw[i]}
		}
		check(client.BatchCallContext(ctx, batch))
		for i, n := range heights[start:end] {
			check(batch[i].Error)
			var h types.Header
			check(json.Unmarshal(raw[i], &h))
			var named struct{ Hash string }
			check(json.Unmarshal(raw[i], &named))
			if h.Hash().Hex() != named.Hash {
				panic(fmt.Sprintf("header hash mismatch at %d", n))
			}
			data, err := rlp.EncodeToBytes(&h)
			check(err)
			encoded[n] = hex.EncodeToString(data)
			headers = append(headers, headerVector{n, named.Hash, encoded[n]})
		}
	}
	var calls []callVector
	call := func(name string, block uint64, address uint8, input string) {
		var result string
		err := client.CallContext(ctx, &result, "eth_call", map[string]string{"to": fmt.Sprintf("0x%040x", address), "data": "0x" + input, "gas": "0x989680"}, hexutil.EncodeUint64(block))
		if err != nil {
			if rpcErr, ok := err.(rpc.Error); !ok || (rpcErr.ErrorCode() != -32000 && rpcErr.ErrorCode() != 3) {
				panic(err)
			}
		}
		if len(result) >= 2 {
			result = result[2:]
		}
		calls = append(calls, callVector{name, block, address, input, result, err != nil})
	}
	word := func(n uint64) string { return fmt.Sprintf("%064x", n) }
	for _, n := range samples {
		prefix := fmt.Sprintf("block-%d/", n)
		call(prefix+"number", latest, 0xf7, encoded[n])
		call(prefix+"hash", latest, 0xf6, encoded[n])
		call(prefix+"seal", latest, 0xf4, encoded[n])
		call(prefix+"validators", latest, 0xf9, word(n))
		call(prefix+"validator-first", latest, 0xfa, word(0)+word(n))
		call(prefix+"validator-key", latest, 0xe1, word(0)+word(n))
	}
	for _, n := range []uint64{0, 1, latest - 4*17280, latest - 4*17280 + 1, latest, latest + 1} {
		call(fmt.Sprintf("parent-seal/%d", n), latest, 0xf5, word(n))
	}
	call("future-header-seal", latest-1, 0xf4, encoded[latest])
	call("genesis-count", latest, 0xf9, word(0))
	call("genesis-validator", latest, 0xfa, word(0)+word(0))
	call("future-count", latest, 0xf9, word(latest+1))
	call("bad-validator-index", latest, 0xfa, word(10000)+word(latest))
	// Parent-seal boundary cases need these additional canonical headers.
	for _, n := range []uint64{latest - 4*17280 + 1} {
		var h types.Header
		check(client.CallContext(ctx, &h, "eth_getBlockByNumber", hexutil.EncodeUint64(n), false))
		data, err := rlp.EncodeToBytes(&h)
		check(err)
		headers = append(headers, headerVector{n, h.Hash().Hex(), hex.EncodeToString(data)})
	}
	result := struct {
		Headers []headerVector `json:"headers"`
		Calls   []callVector   `json:"calls"`
	}{headers, calls}
	data, err := json.MarshalIndent(result, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[2], append(data, '\n'), 0644))
	fmt.Printf("recorded %d native headers and %d consensus calls\n", len(headers), len(calls))
}
