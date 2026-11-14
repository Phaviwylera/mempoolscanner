// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script} from "forge-std/Script.sol";
import {console2} from "forge-std/console2.sol";
import {AaveV3Liquidator} from "../src/AaveV3Liquidator.sol";

// Mainnet:
//   OPERATOR_ADDRESS=0xYourEOA \
//   forge script script/DeployLiquidator.s.sol \
//     --rpc-url $MAINNET_RPC_URL --broadcast --verify \
//     --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY
//
// Sepolia:
//   OPERATOR_ADDRESS=0xYourEOA \
//   forge script script/DeployLiquidator.s.sol \
//     --rpc-url $SEPOLIA_RPC_URL --broadcast --verify \
//     --verifier etherscan --etherscan-api-key $ETHERSCAN_API_KEY
//
// Both chains use Uniswap SwapRouter02 — one struct, no deadline field.

contract DeployLiquidator is Script {
    // ---- Verified constants (source: Aave Address Book + Uniswap docs) ----
    // Mainnet
    address constant MAINNET_POOL_ADDRESSES_PROVIDER = 0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e;
    address constant MAINNET_SWAP_ROUTER02            = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;

    // Sepolia
    address constant SEPOLIA_POOL_ADDRESSES_PROVIDER = 0x012bAC54348C0E635dCAc9D5FB99f06F24136C9A;
    address constant SEPOLIA_SWAP_ROUTER02            = 0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E;

    function run() external returns (AaveV3Liquidator deployed) {
        uint256 chainId = block.chainid;
        address provider;
        address router;

        if (chainId == 1) {
            provider = MAINNET_POOL_ADDRESSES_PROVIDER;
            router   = MAINNET_SWAP_ROUTER02;
        } else if (chainId == 11155111) {
            provider = SEPOLIA_POOL_ADDRESSES_PROVIDER;
            router   = SEPOLIA_SWAP_ROUTER02;
        } else {
            revert("UNSUPPORTED_CHAIN");
        }

        address operator = vm.envAddress("OPERATOR_ADDRESS");
        require(operator != address(0), "OPERATOR_ZERO");

        vm.startBroadcast();
        deployed = new AaveV3Liquidator(provider, router, operator);
        vm.stopBroadcast();

        console2.log("AaveV3Liquidator:", address(deployed));
        console2.log("chainId:       ", chainId);
        console2.log("provider:      ", provider);
        console2.log("swapRouter02:  ", router);
        console2.log("operator:      ", operator);
    }
}
