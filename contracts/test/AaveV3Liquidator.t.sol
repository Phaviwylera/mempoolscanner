// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {forge-std/Test.sol};
import {forge-std/console.sol};
import {AaveV3Liquidator} from "../src/AaveV3Liquidator.sol";
import {IPoolAddressesProvider} from "../src/interfaces/IPoolAddressesProvider.sol";
import {IPool} from "../src/interfaces/IPool.sol";
import {IERC20} from "../src/interfaces/IERC20.sol";

/**
 * @title AaveV3LiquidatorTest
 * Fork test against Ethereum mainnet.
 *
 * Storage slot hack documentation:
 *   For ERC20 `balanceOf` mappings following OpenZeppelin layout:
 *     - `balances` mapping is at storage slot 1
 *     - `balances[user]` is at keccak256(abi.encode(user, uint256(1)))
 *   This is used with vm.store to force a borrower's health factor below 1e18.
 *   NOTE: Aave V3 aTokens/VariableDebtTokens may use different storage layouts.
 *   Adjust the slot computation if the fork test reverts on storage reads.
 */
contract AaveV3LiquidatorTest is Test {
    // Aave V3 Mainnet Addresses
    address constant POOL_ADDRESSES_PROVIDER = 0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e;
    address constant SWAP_ROUTER02           = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;

    // WETH / USDC pair
    address constant WETH      = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC      = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant A_WETH    = 0x030ba81f1c18d280636f32af80b58ad0c05b9e8c;
    address constant VAR_DEBT_USDC = 0x614e509eb0Be5b0144a793C9b12b953c1f89518b;

    address public borrower = address(0x1234567890123456789012345678901234567890);
    address public random   = address(0xdeadbeef);

    AaveV3Liquidator public liquidator;
    IPool public pool;

    function setUp() public {
        vm.createSelectFork(vm.envString("MAINNET_RPC_URL"));

        pool = IPool(IPoolAddressesProvider(POOL_ADDRESSES_PROVIDER).getPool());

        liquidator = new AaveV3Liquidator(POOL_ADDRESSES_PROVIDER, SWAP_ROUTER02, address(this));

        // The owner (this contract) is also the executor for tests.
        liquidator.setExecutor(address(this));

        // --- Force borrower to be unhealthy via storage manipulation ---
        // Set USDC variable debt balance very high (1000 USDC = 1000 * 10^6)
        uint256 debtSlot = keccak256(abi.encode(borrower, uint256(1)));
        vm.store(VAR_DEBT_USDC, debtSlot, 1000 * 10**6);

        // Set borrower's aWETH balance to 0 (no collateral)
        // aWETH balance mapping slot (assuming OZ ERC20 layout, slot 1)
        uint256 collateralSlot = keccak256(abi.encode(borrower, uint256(1)));
        vm.store(A_WETH, collateralSlot, 0);

        // Verify the borrower is unhealthy before attempting liquidation.
        (, , , uint256 healthFactor) = pool.getUserAccountData(borrower);
        assert(healthFactor < 1e18, "Borrower must be unhealthy (HF < 1e18)");
    }

    function test_executeLiquidation_succeeds() public {
        uint256 balanceBefore = IERC20(USDC).balanceOf(address(this));

        // 5000 USDC debt to cover, 3000 fee tier (0.3%), 0 min profit
        liquidator.executeLiquidation(borrower, WETH, USDC, 100 * 10**6, 3000, 0);

        uint256 balanceAfter = IERC20(USDC).balanceOf(address(this));
        assert(balanceAfter > balanceBefore, "Owner debt asset balance must increase");

        // Borrower health factor should have recovered.
        (, , , uint256 healthFactor) = pool.getUserAccountData(borrower);
        assert(healthFactor >= 1e18, "Borrower HF should recover above 1e18");
    }

    function test_executeLiquidation_revertsForNonExecutor() public {
        address nonExecutor = address(0x5555);
        vm.prank(nonExecutor);
        vm.expectRevert("NOT_EXECUTOR");
        liquidator.executeLiquidation(borrower, WETH, USDC, 100 * 10**6, 3000, 0);
    }

    function test_executeOperation_revertsForNonPool() public {
        bytes memory params = abi.encode(borrower, WETH, USDC, 3000, 0);
        vm.prank(random);
        vm.expectRevert("CALLER_NOT_POOL");
        liquidator.executeOperation(USDC, 100 * 10**6, 0, random, params);
    }

    function test_rescueTokens_onlyOwner() public {
        address mockToken = address(0x1111111111111111111111111111111111111111);
        // Deal some mock token balance to the contract
        vm.deal(mockToken, address(liquidator), 1000);

        address nonOwner = address(0x9999);
        vm.prank(nonOwner);
        vm.expectRevert();
        liquidator.rescueTokens(mockToken, 100, nonOwner);
    }

    function test_constructor_rejectsZeroProvider() public {
        vm.expectRevert("ZERO_PROVIDER");
        new AaveV3Liquidator(address(0), SWAP_ROUTER02, address(this));
    }

    function test_constructor_rejectsZeroRouter() public {
        vm.expectRevert("ZERO_ROUTER");
        new AaveV3Liquidator(POOL_ADDRESSES_PROVIDER, address(0), address(this));
    }

    function test_constructor_rejectsZeroOwner() public {
        vm.expectRevert("ZERO_OWNER");
        new AaveV3Liquidator(POOL_ADDRESSES_PROVIDER, SWAP_ROUTER02, address(0));
    }
}
