// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IPoolAddressesProvider {
    function getPool() external view returns (address);
}
