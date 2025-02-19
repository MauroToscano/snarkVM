// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

use super::*;

impl<N: Network> FromBytes for StatePath<N> {
    /// Reads the path from a buffer.
    fn read_le<R: Read>(mut reader: R) -> IoResult<Self> {
        // Read the version.
        let version = u16::read_le(&mut reader)?;
        // Ensure the version is valid.
        if version != 0 {
            return Err(error("Invalid block version"));
        }

        // Read the state path.
        let state_root = N::StateRoot::read_le(&mut reader)?;
        let block_path = BlockPath::read_le(&mut reader)?;
        let block_hash = N::BlockHash::read_le(&mut reader)?;
        let previous_block_hash = N::BlockHash::read_le(&mut reader)?;
        let header_root = Field::read_le(&mut reader)?;
        let header_path = HeaderPath::read_le(&mut reader)?;
        let header_leaf = HeaderLeaf::read_le(&mut reader)?;
        let transactions_path = TransactionsPath::read_le(&mut reader)?;
        let transaction_id = FromBytes::read_le(&mut reader)?;
        let transaction_path = FromBytes::read_le(&mut reader)?;
        let transaction_leaf = FromBytes::read_le(&mut reader)?;
        let transition_path = FromBytes::read_le(&mut reader)?;
        let transition_leaf = FromBytes::read_le(&mut reader)?;

        // Construct the state path.
        Self::new(
            state_root,
            block_path,
            block_hash,
            previous_block_hash,
            header_root,
            header_path,
            header_leaf,
            transactions_path,
            transaction_id,
            transaction_path,
            transaction_leaf,
            transition_path,
            transition_leaf,
        )
        .map_err(|e| error(e.to_string()))
    }
}

impl<N: Network> ToBytes for StatePath<N> {
    /// Writes the path to a buffer.
    fn write_le<W: Write>(&self, mut writer: W) -> IoResult<()> {
        // Write the version.
        0u16.write_le(&mut writer)?;

        // Write the state path.
        self.state_root.write_le(&mut writer)?;
        self.block_path.write_le(&mut writer)?;
        self.block_hash.write_le(&mut writer)?;
        self.previous_block_hash.write_le(&mut writer)?;
        self.header_root.write_le(&mut writer)?;
        self.header_path.write_le(&mut writer)?;
        self.header_leaf.write_le(&mut writer)?;
        self.transactions_path.write_le(&mut writer)?;
        self.transaction_id.write_le(&mut writer)?;
        self.transaction_path.write_le(&mut writer)?;
        self.transaction_leaf.write_le(&mut writer)?;
        self.transition_path.write_le(&mut writer)?;
        self.transition_leaf.write_le(&mut writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::network::Testnet3;

    type CurrentNetwork = Testnet3;

    #[test]
    fn test_bytes() {
        // Sample a ledger.
        let ledger = crate::ledger::test_helpers::sample_genesis_ledger();

        // Retrieve the genesis block.
        let genesis = ledger.get_block(0).unwrap();
        // Ensure there is at least 1 commitment.
        assert!(genesis.transactions().commitments().count() > 0);

        // Check each commitment.
        for commitment in genesis.transactions().commitments() {
            // Compute the state path.
            let expected = ledger.to_state_path(commitment).unwrap();

            // Check the byte representation.
            let expected_bytes = expected.to_bytes_le().unwrap();
            assert_eq!(expected, StatePath::read_le(&expected_bytes[..]).unwrap());
            assert!(StatePath::<CurrentNetwork>::read_le(&expected_bytes[1..]).is_err());
        }
    }
}
