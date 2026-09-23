// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
pragma solidity 0.8.28;

import {HyparbExecutor, IClPool} from "../src/HyparbExecutor.sol";

interface Vm {
    function deal(address who, uint256 amount) external;
}

interface IWhype {
    function deposit() external payable;
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address who) external view returns (uint256);
}

interface IErc20 {
    function balanceOf(address who) external view returns (uint256);
}

/// Against the REAL contracts, on a HyperEVM mainnet fork — one pool per
/// family. Run: `forge test --fork-url <HyperEVM RPC> --match-contract Fork`.
/// Without a fork the pools have no code and every test returns early.
contract HyparbExecutorForkTest {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);
    address constant WHYPE = 0x5555555555555555555555555555555555555555;
    uint160 constant MIN_SQRT_RATIO = 4295128739;
    uint160 constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342;

    /// Uniswap V3 ABI (WHYPE/USDC, 0.05 %).
    address constant V3_POOL = 0x6c9A33E3b592C0d65B3Ba59355d5Be0d38259285;
    /// Slipstream / Hybra CL.
    address constant SLIPSTREAM_POOL = 0xC22FaD66665343D385608cC45D2e1484f9bA8D6b;
    /// Algebra Integral v1.0 (NEST).
    address constant ALGEBRA_POOL = 0x20e6E73C91a29d21BdE672562a4B16649D66623E;
    /// Algebra Integral v1.2 (Kittenswap, plugin fees).
    address constant ALGEBRA_V12_POOL = 0x12Df9913E9E08453440e3C4B1aE73819160b513E;
    /// Hyperswap V3 (WHYPE/USDT0, 0.05 %; factory 0xB1c0…02E3) — calls
    /// `hyperswapV3SwapCallback`.
    address constant HYPERSWAP_POOL = 0x337b56d87A6185cD46AF3Ac2cDF03CBC37070C30;

    receive() external payable {}

    function _sell_whype(address pool) internal {
        if (pool.code.length == 0) return; // no fork
        HyparbExecutor x = new HyparbExecutor();
        address t0 = IClPool(pool).token0();
        address t1 = IClPool(pool).token1();
        require(t0 == WHYPE || t1 == WHYPE, "pool must pair WHYPE");
        vm.deal(address(this), 2 ether);
        IWhype(WHYPE).deposit{value: 1 ether}();
        require(IWhype(WHYPE).transfer(address(x), 1 ether), "fund");
        bool zeroForOne = t0 == WHYPE;
        address tout = zeroForOne ? t1 : t0;
        uint160 limit = zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1;

        (bool ok, bytes memory ret) =
            address(x).call(abi.encodeCall(HyparbExecutor.swap, (pool, zeroForOne, int256(0.01 ether), limit, type(uint256).max)));
        bytes4 sel;
        assembly {
            sel := mload(add(ret, 32))
        }
        require(!ok && sel == HyparbExecutor.BelowMinOut.selector, "an unreachable minOut must revert BelowMinOut");
        require(IWhype(WHYPE).balanceOf(address(x)) == 1 ether, "the reverted swap moved nothing");

        (int256 a0, int256 a1) = x.swap(pool, zeroForOne, int256(0.01 ether), limit, 1);
        int256 paid = zeroForOne ? a0 : a1;
        int256 got = zeroForOne ? a1 : a0;
        require(paid == int256(0.01 ether), "exact input paid");
        require(got < 0, "output received");
        require(IWhype(WHYPE).balanceOf(address(x)) == 1 ether - 0.01 ether, "WHYPE debited exactly");
        require(IErc20(tout).balanceOf(address(x)) == uint256(-got), "output credited exactly");
    }

    function test_fork_uniswap_v3() public {
        _sell_whype(V3_POOL);
    }

    function test_fork_slipstream() public {
        _sell_whype(SLIPSTREAM_POOL);
    }

    function test_fork_algebra() public {
        _sell_whype(ALGEBRA_POOL);
    }

    function test_fork_algebra_v12() public {
        _sell_whype(ALGEBRA_V12_POOL);
    }

    function test_fork_hyperswap_v3() public {
        _sell_whype(HYPERSWAP_POOL);
    }
}
