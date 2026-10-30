// Run from the Celo Go module with its native BLS library:
// go run /path/to/leafage/scripts/gen_celo_l1_vectors.go input.json output.json keys.json
// Rechecks upstream vectors and records RequiredGas from the actual Celo code.
package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"strings"

	"github.com/celo-org/celo-blockchain/common"
	"github.com/celo-org/celo-blockchain/core/vm"
	"github.com/celo-org/celo-blockchain/params"
	"github.com/celo-org/celo-bls-go/bls"
)

type vector struct {
	Name        string `json:"name"`
	Address     uint8  `json:"address"`
	Gingerbread bool   `json:"gingerbread"`
	Input       string `json:"input"`
	Expected    string `json:"expected"`
	Error       bool   `json:"error"`
	Gas         uint64 `json:"gas"`
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func save(name string, v interface{}) {
	data, err := json.MarshalIndent(v, "", "  ")
	must(err)
	must(os.WriteFile(name, append(data, '\n'), 0644))
}

func main() {
	data, err := os.ReadFile(os.Args[1])
	must(err)
	var vectors []vector
	must(json.Unmarshal(data, &vectors))
	// Exercise the legacy GLV implementation with arbitrary curve points and
	// full-width scalars, including points from the pairing rejection corpus.
	original := append([]vector(nil), vectors...)
	for _, spec := range []struct {
		mul, pair    uint8
		offset, size int
	}{{0xf1, 0xec, 0, 128}, {0xee, 0xec, 128, 256}, {0xe8, 0xe3, 0, 128}, {0xe5, 0xe3, 128, 256}} {
		points := make(map[string]bool)
		for _, v := range original {
			b := common.Hex2Bytes(v.Input)
			var point []byte
			if v.Address == spec.mul && len(b) == spec.size+32 {
				point = b[:spec.size]
			}
			if v.Address == spec.pair && len(b) >= 384 && strings.Contains(strings.ToLower(v.Name), "subgroup") {
				point = b[spec.offset : spec.offset+spec.size]
			}
			if point == nil || points[hex.EncodeToString(point)] {
				continue
			}
			points[hex.EncodeToString(point)] = true
			for _, value := range []*big.Int{big.NewInt(0), big.NewInt(1), new(big.Int).Lsh(big.NewInt(1), 255), new(big.Int).Sub(new(big.Int).Lsh(big.NewInt(1), 256), big.NewInt(1))} {
				input := append(append([]byte(nil), point...), common.LeftPadBytes(value.Bytes(), 32)...)
				vectors = append(vectors, vector{Name: fmt.Sprintf("native-edge/glv-%x/%d/%x", spec.mul, len(points), value), Address: spec.mul, Input: hex.EncodeToString(input)})
			}
		}
	}
	// Celo uses ed25519consensus. These distinguish ZIP-215 from Go's stdlib.
	var encodings [][]byte
	for _, first := range []byte{0, 1, 0xed, 0xee} {
		b := make([]byte, 32)
		b[0] = first
		if first >= 0xed {
			for i := 1; i < 32; i++ {
				b[i] = 255
			}
			b[31] = 127
		}
		encodings = append(encodings, b)
	}
	for i, public := range encodings {
		for j, r := range encodings {
			input := append(append(append(append([]byte(nil), public...), r...), make([]byte, 32)...), []byte("Celo")...)
			vectors = append(vectors, vector{Name: fmt.Sprintf("native-edge/ed25519/%d/%d", i, j), Address: 0xf3, Input: hex.EncodeToString(input)})
		}
	}
	var keys []map[string]string
	for _, seed := range []byte{1, 5, 17} {
		// Deliberate public test scalars; these are not account keys.
		raw := make([]byte, 32)
		raw[0] = seed
		secret, err := bls.DeserializePrivateKey(raw)
		must(err)
		public, err := secret.ToPublic()
		must(err)
		address := bytes.Repeat([]byte{seed}, 20)
		signature, err := secret.SignPoP(address)
		must(err)
		pk, err := public.Serialize()
		must(err)
		sig, err := signature.Serialize()
		must(err)
		must(public.VerifyPoP(address, signature))
		input := append(append(address, pk...), sig...)
		vectors = append(vectors, vector{Name: fmt.Sprintf("native-pop/%d", seed), Address: 0xfb, Input: hex.EncodeToString(input), Expected: fmt.Sprintf("%064x", 1)})
		corrupt := append([]byte(nil), input...)
		corrupt[0] ^= 1
		vectors = append(vectors, vector{Name: fmt.Sprintf("native-pop/wrong-message/%d", seed), Address: 0xfb, Input: hex.EncodeToString(corrupt), Error: true})
		uncompressed, err := public.SerializeUncompressed()
		must(err)
		keys = append(keys, map[string]string{"public_key": hex.EncodeToString(pk), "uncompressed": hex.EncodeToString(uncompressed)})
		signature.Destroy()
		public.Destroy()
		secret.Destroy()
	}
	for i := range vectors {
		v := &vectors[i]
		input := common.Hex2Bytes(v.Input)
		v.Input = hex.EncodeToString(input)
		p := vm.PrecompiledContractsDonut[common.BytesToAddress([]byte{v.Address})]
		if p == nil {
			panic(v.Name)
		}
		height := int64(10_000_000)
		if v.Gingerbread {
			height = 31_056_499
		}
		evm := vm.NewEVM(vm.BlockContext{BlockNumber: big.NewInt(height)}, vm.TxContext{}, nil, params.MainnetChainConfig, vm.Config{})
		v.Gas = p.RequiredGas(input)
		output, _, err := vm.RunPrecompiledContract(p, input, v.Gas, vm.NewContext(common.Address{}, evm))
		if strings.HasPrefix(v.Name, "native-edge/") {
			v.Expected = hex.EncodeToString(output)
			v.Error = err != nil
			continue
		}
		if (err != nil) != v.Error {
			panic(fmt.Sprintf("%s: expected error=%v, got %v", v.Name, v.Error, err))
		}
		if !v.Error && hex.EncodeToString(output) != v.Expected {
			panic(fmt.Sprintf("%s: output mismatch", v.Name))
		}
	}
	save(os.Args[2], vectors)
	save(os.Args[3], keys)
	fmt.Printf("verified %d native vectors\n", len(vectors))
}
