// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";
import {AaveV3Liquidator} from "../src/AaveV3Liquidator.sol";
import {IPoolAddressesProvider} from "../src/interfaces/IPoolAddressesProvider.sol";
import {IPool} from "../src/interfaces/IPool.sol";
import {IERC20} from "../src/interfaces/IERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

/**
 * @title AaveV3LiquidatorTest
 *
 * Fork test against Ethereum mainnet at a block where the target position was
 * genuinely liquidatable.
 *
 * No storage manipulation. Forcing a health factor with `vm.store` does not
 * work on Aave V3 anyway -- aTokens and variable debt tokens hold *scaled*
 * balances and the health factor is derived from the oracle plus reserve
 * config, so overwriting one slot either reverts or produces state the
 * protocol rejects. Worse, it proves nothing about real behaviour.
 *
 * Instead each target below is a real liquidation that happened on mainnet.
 * The fork is pinned one block BEFORE it, so the position is unhealthy in
 * genuine protocol state, and the test performs the same liquidation the real
 * keeper did.
 *
 * Source: `recent_liquidations` from this project's own scanner.
 *   tx 0xd9eca4c5e3cdb685e234896c55a916c2513112dab4193a3b825391d258ee2186
 *   block 26410242
 */
contract AaveV3LiquidatorTest is Test {
    // --- Aave V3 / Uniswap mainnet addresses ---
    address constant POOL_ADDRESSES_PROVIDER = 0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e;
    address constant SWAP_ROUTER02 = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;

    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;

    // --- The real liquidation we replay ---
    uint256 constant LIQUIDATION_BLOCK = 26_410_242;

    /// The real liquidation transaction. Forking *at this tx* replays every
    /// earlier transaction in its block and stops just before it runs.
    ///
    /// Forking at LIQUIDATION_BLOCK - 1 does NOT work, and the reason matters:
    /// the oracle update that pushed this position underwater landed in the
    /// same block as the liquidation, so at the end of the previous block the
    /// health factor was still 1.00334 -- healthy. Liquidations routinely
    /// share a block with the price update that caused them.
    bytes32 constant LIQUIDATION_TX =
        0xd9eca4c5e3cdb685e234896c55a916c2513112dab4193a3b825391d258ee2186;

    // 1,148.00 USDC of debt was covered, seizing 0.634832 WETH (~5% bonus).
    address constant TARGET = 0x0DFA39fBd4c01Da42C0cdb835D943d31f2c9c5c9;
    uint256 constant DEBT_TO_COVER = 1_148 * 1e6; // USDC has 6 decimals

    // WETH/USDC 0.05% tier is the deep pool for this pair.
    uint24 constant POOL_FEE = 500;

    AaveV3Liquidator internal liquidator;
    IPool internal pool;

    function setUp() public {
        vm.createSelectFork(vm.envString("MAINNET_RPC_URL"), LIQUIDATION_TX);

        pool = IPool(IPoolAddressesProvider(POOL_ADDRESSES_PROVIDER).getPool());

        liquidator = new AaveV3Liquidator(
            POOL_ADDRESSES_PROVIDER,
            SWAP_ROUTER02,
            address(this)
        );
    }

    /// The whole point: flash loan -> liquidate -> swap -> repay -> profit.
    function test_flashLoanLiquidation_succeedsAndPaysOwner() public {
        (, , , , , uint256 hfBefore) = pool.getUserAccountData(TARGET);
        console2.log("fork block                :", block.number);
        console2.log("target health factor (wei):", hfBefore);
        assertLt(hfBefore, 1e18, "target must be liquidatable in real state");

        uint256 ownerUsdcBefore = IERC20(USDC).balanceOf(address(this));
        assertEq(
            IERC20(USDC).balanceOf(address(liquidator)),
            0,
            "liquidator starts with no capital of its own"
        );

        liquidator.executeLiquidation(
            TARGET,
            WETH,
            USDC,
            DEBT_TO_COVER,
            POOL_FEE,
            0 // minProfit: assert on the realised number instead
        );

        uint256 ownerUsdcAfter = IERC20(USDC).balanceOf(address(this));
        uint256 profit = ownerUsdcAfter - ownerUsdcBefore;

        console2.log("debt covered        (USDC):", DEBT_TO_COVER);
        console2.log("profit to owner     (USDC):", profit);

        assertGt(profit, 0, "liquidation must leave a profit for the owner");

        // The contract must not retain funds: everything is swept each run.
        assertEq(
            IERC20(USDC).balanceOf(address(liquidator)),
            0,
            "no debt asset left in the contract"
        );
        assertEq(
            IERC20(WETH).balanceOf(address(liquidator)),
            0,
            "all seized collateral was swapped"
        );

        // Repaying debt must improve the borrower's health factor.
        (, , , , , uint256 hfAfter) = pool.getUserAccountData(TARGET);
        console2.log("health factor after  (wei):", hfAfter);
        assertGt(hfAfter, hfBefore, "liquidation must raise the health factor");
    }

    /// minProfit is enforced by the swap's amountOutMinimum, so an absurd
    /// floor must abort the whole atomic bundle rather than run at a loss.
    function test_unreachableMinProfit_reverts() public {
        vm.expectRevert();
        liquidator.executeLiquidation(
            TARGET,
            WETH,
            USDC,
            DEBT_TO_COVER,
            POOL_FEE,
            1_000_000 * 1e6 // 1M USDC of profit is not happening
        );
    }

    // --- access control ---

    function test_executeLiquidation_revertsForNonExecutor() public {
        vm.prank(address(0x5555));
        vm.expectRevert("NOT_EXECUTOR");
        liquidator.executeLiquidation(TARGET, WETH, USDC, DEBT_TO_COVER, POOL_FEE, 0);
    }

    function test_executeOperation_revertsForNonPool() public {
        bytes memory params = abi.encode(TARGET, WETH, USDC, POOL_FEE, uint256(0));
        vm.prank(address(0xBEEF));
        vm.expectRevert("CALLER_NOT_POOL");
        liquidator.executeOperation(USDC, DEBT_TO_COVER, 0, address(0xBEEF), params);
    }

    /// Only the pool may call back, and only for a loan this contract started.
    function test_executeOperation_revertsForForeignInitiator() public {
        bytes memory params = abi.encode(TARGET, WETH, USDC, POOL_FEE, uint256(0));
        vm.prank(address(pool));
        vm.expectRevert("INITIATOR_NOT_SELF");
        liquidator.executeOperation(USDC, DEBT_TO_COVER, 0, address(0xBEEF), params);
    }

    function test_setExecutor_onlyOwner() public {
        vm.prank(address(0x9999));
        vm.expectRevert();
        liquidator.setExecutor(address(0x9999));

        liquidator.setExecutor(address(0x4242));
        assertEq(liquidator.executor(), address(0x4242));
    }

    function test_rescueTokens_onlyOwner() public {
        deal(USDC, address(liquidator), 1_000e6);

        vm.prank(address(0x9999));
        vm.expectRevert();
        liquidator.rescueTokens(USDC, 1_000e6, address(0x9999));

        liquidator.rescueTokens(USDC, 1_000e6, address(this));
        assertEq(IERC20(USDC).balanceOf(address(liquidator)), 0);
    }

    // --- constructor guards ---

    function test_constructor_rejectsZeroProvider() public {
        vm.expectRevert("ZERO_PROVIDER");
        new AaveV3Liquidator(address(0), SWAP_ROUTER02, address(this));
    }

    function test_constructor_rejectsZeroRouter() public {
        vm.expectRevert("ZERO_ROUTER");
        new AaveV3Liquidator(POOL_ADDRESSES_PROVIDER, address(0), address(this));
    }

    /// A zero owner IS rejected, but by OpenZeppelin rather than by the
    /// contract's own require: the `Ownable(initialOwner)` base constructor
    /// runs before the constructor body, so OwnableInvalidOwner fires first.
    /// The ZERO_OWNER require is therefore unreachable, which is fine -- the
    /// guarantee holds either way.
    function test_constructor_rejectsZeroOwner() public {
        vm.expectRevert(
            abi.encodeWithSelector(Ownable.OwnableInvalidOwner.selector, address(0))
        );
        new AaveV3Liquidator(POOL_ADDRESSES_PROVIDER, SWAP_ROUTER02, address(0));
    }
}
