use beserial::{Serialize, Deserialize};
use crate::tree::AccountsTreeNode;
use crate::tree::AccountsTreeNodeChild;
use crate::tree::AddressNibbles;
use primitives::account::BasicAccount;
use primitives::account::Account;
use hash::Hash;
use hash::Blake2bHash;
use keys::Address;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountsProof {
    #[beserial(len_type(u16))]
    nodes: Vec<AccountsTreeNode>,
    #[beserial(skip)]
    verified: bool
}

impl AccountsProof {
    pub(crate) fn new(nodes : Vec<AccountsTreeNode>) -> AccountsProof {
        return AccountsProof { nodes, verified: false };
    }

    pub fn verify(&mut self) -> bool {
        let mut children: Vec<AccountsTreeNode> = Vec::new();
        for node in &self.nodes {
            // If node is a branch node, validate its children.
            if node.is_branch() {
                while let Some(child) = children.pop() {
                    if node.prefix().is_prefix_of(child.prefix()) {
                        let hash = child.hash::<Blake2bHash>();
                        // If the child is not valid, return false.
                        if node.get_child_hash(child.prefix()).unwrap() != &hash || &node.get_child_prefix(child.prefix()).unwrap() != child.prefix() {
                            return false;
                        }
                    } else {
                        children.push(child);
                        break;
                    }
                }
            }
            children.push(node.clone());
        }
        let root_nibbles : AddressNibbles = "".parse().unwrap();
        let valid = children.len() == 1 && children[0].prefix() == &root_nibbles && children[0].is_branch();
        self.verified = valid;
        return valid;
    }

    pub fn get_account(&self, address: &Address) -> Option<Account> {
        assert!(self.verified, "AccountsProof must be verified before retrieving accounts. Call verify() first.");

        for node in &self.nodes {
            if let AccountsTreeNode::TerminalNode { prefix, account } = node {
                if prefix == &AddressNibbles::from(address) {

                    return Some(account.clone());
                }
            }
        }
        return None;
    }

    pub fn root_hash(&self) -> Blake2bHash {
        return (&self.nodes[self.nodes.len() - 1]).hash();
    }
}

#[test]
fn it_can_verify() {
    /*
     * We're going to construct three proofs based on this tree:
     *
     *      R1
     *      |
     *      B1
     *    / |  \
     *   T1 B2 T2
     *     / \
     *    T3 T4
     */

    let an1: AddressNibbles = "0011111111111111111111111111111111111111".parse().unwrap();
    let account1 = Account::Basic(BasicAccount { balance: 25.into() });
    let address1 = Address::from(hex::decode(an1.to_string()).unwrap().as_slice());
    let t1 = AccountsTreeNode::new_terminal(an1, account1.clone());

    let an2: AddressNibbles = "0033333333333333333333333333333333333333".parse().unwrap();
    let account2 = Account::Basic(BasicAccount { balance: 1.into() });
    let address2 = Address::from(hex::decode(an2.to_string()).unwrap().as_slice());
    let t2 = AccountsTreeNode::new_terminal(an2, account2.clone());

    let an3: AddressNibbles = "0020000000000000000000000000000000000000".parse().unwrap();
    let account3 = Account::Basic(BasicAccount { balance: 1322.into() });
    let address3 = Address::from(hex::decode(an3.to_string()).unwrap().as_slice());
    let t3 = AccountsTreeNode::new_terminal(an3, account3.clone());

    let an4: AddressNibbles = "0022222222222222222222222222222222222222".parse().unwrap();
    let account4 = Account::Basic(BasicAccount { balance: 93.into() });
    let address4 = Address::from(hex::decode(an4.to_string()).unwrap().as_slice());
    let t4 = AccountsTreeNode::new_terminal(an4, account4.clone());

    let b2 = AccountsTreeNode::new_branch("002".parse().unwrap(), [
        Some(AccountsTreeNodeChild { suffix: "0000000000000000000000000000000000000".parse().unwrap(), hash: t3.hash() }), None,
        Some(AccountsTreeNodeChild { suffix: "2222222222222222222222222222222222222".parse().unwrap(), hash: t4.hash() }),
        None, None, None, None, None, None, None, None, None, None, None, None, None]);

    let b1 = AccountsTreeNode::new_branch("00".parse().unwrap(), [ None,
        Some(AccountsTreeNodeChild { suffix: "11111111111111111111111111111111111111".parse().unwrap(), hash: t1.hash() }),
        Some(AccountsTreeNodeChild { suffix: "2".parse().unwrap(), hash: b2.hash() }),
        Some(AccountsTreeNodeChild { suffix: "33333333333333333333333333333333333333".parse().unwrap(), hash: t2.hash() }),
        None, None, None, None, None, None, None, None, None, None, None, None] );

    let r1 = AccountsTreeNode::new_branch("".parse().unwrap(), [
        Some(AccountsTreeNodeChild { suffix: "00".parse().unwrap(), hash: b1.hash() }),
        None, None, None, None, None, None, None, None, None, None, None, None, None, None, None]);

    // The first proof proves the 4 terminal nodes (T1, T2, T3 and T4)
    let mut proof1 = AccountsProof::new(vec![t1.clone(), t3.clone(), t4.clone(), b2.clone(), t2.clone(), b1.clone(), r1.clone()]);
    assert!(proof1.verify());
    assert_eq!(account1, proof1.get_account(&address1).unwrap());
    assert_eq!(account2, proof1.get_account(&address2).unwrap());
    assert_eq!(account3, proof1.get_account(&address3).unwrap());
    assert_eq!(account4, proof1.get_account(&address4).unwrap());

    // The second proof proves the 2 leftmost terminal nodes (T1 and T3)
    let mut proof2 = AccountsProof::new(vec![t1.clone(), t3.clone(), b2.clone(), b1.clone(), r1.clone()]);
    assert!(proof2.verify());
    assert_eq!(account1, proof2.get_account(&address1).unwrap());
    assert_eq!(account3, proof2.get_account(&address3).unwrap());
    assert_eq!(None, proof2.get_account(&address2));
    assert_eq!(None, proof2.get_account(&address4));

    // The third proof just proves T4
    let mut proof3 = AccountsProof::new(vec![t4.clone(), b2.clone(), b1.clone(), r1.clone()]);
    assert!(proof3.verify());
    assert_eq!(account4, proof3.get_account(&address4).unwrap());
    assert_eq!(None, proof3.get_account(&address1));
    assert_eq!(None, proof3.get_account(&address2));
    assert_eq!(None, proof3.get_account(&address3));

    // must return the correct root hash
    assert!(proof1.root_hash() == r1.hash());
}
